//! 文件传输：测试帧 payload 内的 META / DATA / ACK，以及接收侧 MD5 校验状态机。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use md5::{Digest, Md5};

use crate::frame::{encode, inspect, FrameView, OVERHEAD};

pub const MSG_META: u8 = 0x01;
pub const MSG_DATA: u8 = 0x02;
pub const MSG_ACK: u8 = 0x03;
pub const ACK_OK: u8 = 0;
pub const ACK_FAIL: u8 = 1;
pub const MD5_LEN: usize = 16;
pub const DEFAULT_CHUNK: usize = 1024;
/// 与 main 空闲组帧上限一致：一帧 wire 不超过 8192。
pub const MAX_IDLE_FRAME: usize = 8192;
const DATA_HDR: usize = 1 + 4 + 4;
/// DATA 数据区最大字节（payload + 10 字节帧开销 ≤ 8192）。
pub const MAX_CHUNK: usize = MAX_IDLE_FRAME - OVERHEAD - DATA_HDR;

const META_FIXED: usize = 1 + 4 + 8 + 4 + MD5_LEN + 1;
const ACK_LEN: usize = 1 + 4 + 1 + MD5_LEN;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileMsg {
    Meta {
        xfer_id: u32,
        file_size: u64,
        chunk_count: u32,
        md5: [u8; MD5_LEN],
        name: String,
    },
    Data {
        xfer_id: u32,
        chunk_idx: u32,
        data: Vec<u8>,
    },
    Ack {
        xfer_id: u32,
        result: u8,
        got_md5: [u8; MD5_LEN],
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgError {
    Empty,
    UnknownType,
    Truncated,
    NameTooLong,
}

pub fn chunk_count(file_size: u64, chunk: usize) -> Result<u32, &'static str> {
    if chunk == 0 || chunk > MAX_CHUNK {
        return Err("分片大小非法");
    }
    if file_size == 0 {
        return Err("文件为空");
    }
    let n = (file_size + chunk as u64 - 1) / chunk as u64;
    if n > u32::MAX as u64 {
        return Err("分片数量超过 u32");
    }
    Ok(n as u32)
}

#[cfg(test)]
pub fn md5_bytes(data: &[u8]) -> [u8; MD5_LEN] {
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher.finalize().into()
}

pub fn md5_file(path: &Path) -> io::Result<[u8; MD5_LEN]> {
    let mut file = File::open(path)?;
    let mut hasher = Md5::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

pub fn md5_hex(md5: &[u8; MD5_LEN]) -> String {
    md5.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_md5_hex(s: &str) -> Result<[u8; MD5_LEN], &'static str> {
    let t = s.trim();
    if t.len() != 32 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("MD5 应为 32 位十六进制");
    }
    let mut out = [0u8; MD5_LEN];
    for i in 0..MD5_LEN {
        out[i] = u8::from_str_radix(&t[i * 2..i * 2 + 2], 16).map_err(|_| "MD5 解析失败")?;
    }
    Ok(out)
}

pub fn encode_msg(msg: &FileMsg) -> Result<Vec<u8>, MsgError> {
    match msg {
        FileMsg::Meta {
            xfer_id,
            file_size,
            chunk_count,
            md5,
            name,
        } => {
            let name_bytes = name.as_bytes();
            if name_bytes.len() > u8::MAX as usize {
                return Err(MsgError::NameTooLong);
            }
            let mut out = Vec::with_capacity(META_FIXED + name_bytes.len());
            out.push(MSG_META);
            out.extend_from_slice(&xfer_id.to_le_bytes());
            out.extend_from_slice(&file_size.to_le_bytes());
            out.extend_from_slice(&chunk_count.to_le_bytes());
            out.extend_from_slice(md5);
            out.push(name_bytes.len() as u8);
            out.extend_from_slice(name_bytes);
            Ok(out)
        }
        FileMsg::Data {
            xfer_id,
            chunk_idx,
            data,
        } => {
            let mut out = Vec::with_capacity(DATA_HDR + data.len());
            out.push(MSG_DATA);
            out.extend_from_slice(&xfer_id.to_le_bytes());
            out.extend_from_slice(&chunk_idx.to_le_bytes());
            out.extend_from_slice(data);
            Ok(out)
        }
        FileMsg::Ack {
            xfer_id,
            result,
            got_md5,
        } => {
            let mut out = Vec::with_capacity(ACK_LEN);
            out.push(MSG_ACK);
            out.extend_from_slice(&xfer_id.to_le_bytes());
            out.push(*result);
            out.extend_from_slice(got_md5);
            Ok(out)
        }
    }
}

pub fn decode_msg(payload: &[u8]) -> Result<FileMsg, MsgError> {
    if payload.is_empty() {
        return Err(MsgError::Empty);
    }
    match payload[0] {
        MSG_META => {
            if payload.len() < META_FIXED {
                return Err(MsgError::Truncated);
            }
            let xfer_id = u32::from_le_bytes(payload[1..5].try_into().unwrap());
            let file_size = u64::from_le_bytes(payload[5..13].try_into().unwrap());
            let chunk_count = u32::from_le_bytes(payload[13..17].try_into().unwrap());
            let mut md5 = [0u8; MD5_LEN];
            md5.copy_from_slice(&payload[17..33]);
            let name_len = payload[33] as usize;
            if payload.len() != META_FIXED + name_len {
                return Err(MsgError::Truncated);
            }
            let name = String::from_utf8_lossy(&payload[34..34 + name_len]).into_owned();
            Ok(FileMsg::Meta {
                xfer_id,
                file_size,
                chunk_count,
                md5,
                name,
            })
        }
        MSG_DATA => {
            if payload.len() < DATA_HDR {
                return Err(MsgError::Truncated);
            }
            let xfer_id = u32::from_le_bytes(payload[1..5].try_into().unwrap());
            let chunk_idx = u32::from_le_bytes(payload[5..9].try_into().unwrap());
            Ok(FileMsg::Data {
                xfer_id,
                chunk_idx,
                data: payload[9..].to_vec(),
            })
        }
        MSG_ACK => {
            if payload.len() != ACK_LEN {
                return Err(MsgError::Truncated);
            }
            let xfer_id = u32::from_le_bytes(payload[1..5].try_into().unwrap());
            let result = payload[5];
            let mut got_md5 = [0u8; MD5_LEN];
            got_md5.copy_from_slice(&payload[6..22]);
            Ok(FileMsg::Ack {
                xfer_id,
                result,
                got_md5,
            })
        }
        _ => Err(MsgError::UnknownType),
    }
}

pub fn encode_wire(seq: u32, msg: &FileMsg) -> Result<Vec<u8>, &'static str> {
    let payload = encode_msg(msg).map_err(|_| "文件消息编码失败")?;
    if payload.len() + OVERHEAD > MAX_IDLE_FRAME {
        return Err("文件帧超过 8192 字节");
    }
    encode(seq, &payload)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseEvent {
    Msg(FileMsg),
    BadFrame { kind: &'static str, raw: Vec<u8> },
}

pub fn parse_events(raw: &[u8]) -> Vec<ParseEvent> {
    inspect(raw)
        .into_iter()
        .map(|view| match view {
            FrameView::Ok(frame) => match decode_msg(&frame.payload) {
                Ok(msg) => ParseEvent::Msg(msg),
                Err(_) => ParseEvent::BadFrame {
                    kind: "非文件消息",
                    raw: frame.payload,
                },
            },
            FrameView::BadCrc => ParseEvent::BadFrame {
                kind: "CRC错误",
                raw: raw.to_vec(),
            },
            FrameView::NotTest => ParseEvent::BadFrame {
                kind: "非测试帧",
                raw: raw.to_vec(),
            },
        })
        .collect()
}

pub fn parse_msgs(raw: &[u8]) -> Vec<FileMsg> {
    parse_events(raw)
        .into_iter()
        .filter_map(|ev| match ev {
            ParseEvent::Msg(msg) => Some(msg),
            ParseEvent::BadFrame { .. } => None,
        })
        .collect()
}

#[derive(Debug)]
pub struct RecvNote {
    pub xfer_id: u32,
    pub why: String,
    pub expected_idx: u32,
    pub got_idx: u32,
    pub written: u64,
    pub file_size: u64,
    pub missing: u32,
}

impl RecvNote {
    pub fn log_line(&self) -> String {
        format!(
            "file xfer={} 出错 原因={} 期望片={} 收到片={} 已写={}/{} 缺片数={} 继续收本轮",
            self.xfer_id,
            self.why,
            self.expected_idx,
            self.got_idx,
            self.written,
            self.file_size,
            self.missing
        )
    }
}

#[derive(Debug)]
pub struct RecvOutcome {
    pub xfer_id: u32,
    pub passed: bool,
    pub recorded_golden: bool,
    pub got_md5: [u8; MD5_LEN],
    pub expect_md5: Option<[u8; MD5_LEN]>,
    pub kept_path: Option<PathBuf>,
    pub kept_bad: Option<PathBuf>,
    pub why: Option<String>,
    pub written: u64,
    pub file_size: u64,
    pub got_chunks: u32,
    pub chunk_count: u32,
    pub first_gap: Option<u32>,
}

impl RecvOutcome {
    pub fn ack_result(&self) -> u8 {
        if self.passed {
            ACK_OK
        } else {
            ACK_FAIL
        }
    }

    pub fn log_line(&self) -> String {
        let got = md5_hex(&self.got_md5);
        if self.recorded_golden {
            let kept = display_path(&self.kept_path);
            format!(
                "file xfer={} 已记录基准 MD5={} 已保留 {kept}",
                self.xfer_id, got
            )
        } else if self.passed {
            format!("file xfer={} 通过 md5={} 已删除", self.xfer_id, got)
        } else {
            let expect = self
                .expect_md5
                .map(|m| md5_hex(&m))
                .unwrap_or_else(|| "-".to_string());
            let kept = display_path(&self.kept_path);
            let why = self.why.as_deref().unwrap_or("-");
            let gap = self
                .first_gap
                .map(|g| g.to_string())
                .unwrap_or_else(|| "-".to_string());
            let bad = self
                .kept_bad
                .as_ref()
                .map(|p| format!(" 坏帧={}", p.display()))
                .unwrap_or_default();
            format!(
                "file xfer={} 失败 原因={why} expect={expect} got={got} 已收片={}/{} 已写={}/{} 首处缺片={gap} 已保留 {kept}{bad}",
                self.xfer_id,
                self.got_chunks,
                self.chunk_count,
                self.written,
                self.file_size
            )
        }
    }
}

fn display_path(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "未保留".to_string())
}

#[derive(Debug)]
pub enum FeedResult {
    Nothing,
    /// 本轮出错但继续收，不 ACK。
    Note(RecvNote),
    Done(RecvOutcome),
    /// 新 META 打断未完成传输；新传输已开始。
    AbortedPrevious(RecvOutcome),
}

struct InFlight {
    xfer_id: u32,
    file_size: u64,
    chunk_count: u32,
    learned_chunk: Option<u32>,
    received: Vec<bool>,
    got_chunks: u32,
    written: u64,
    path: PathBuf,
    file: File,
    bad_path: PathBuf,
    bad_file: Option<File>,
    last_activity: Instant,
    first_error: Option<String>,
    first_got_idx: u32,
}

pub struct RecvEngine {
    dir: PathBuf,
    expect_md5: Option<[u8; MD5_LEN]>,
    golden: Option<[u8; MD5_LEN]>,
    max_fail_keep: usize,
    current: Option<InFlight>,
}

impl RecvEngine {
    pub fn new(
        dir: PathBuf,
        expect_md5: Option<[u8; MD5_LEN]>,
        max_fail_keep: usize,
    ) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            expect_md5,
            golden: None,
            max_fail_keep,
            current: None,
        })
    }

    #[cfg(test)]
    pub fn golden(&self) -> Option<[u8; MD5_LEN]> {
        self.golden
    }

    #[cfg(test)]
    pub fn inflight(&self) -> bool {
        self.current.is_some()
    }

    pub fn last_activity(&self) -> Option<Instant> {
        self.current.as_ref().map(|c| c.last_activity)
    }

    pub fn feed(&mut self, msg: FileMsg) -> io::Result<FeedResult> {
        match msg {
            FileMsg::Ack { .. } => Ok(FeedResult::Nothing),
            FileMsg::Meta {
                xfer_id,
                file_size,
                chunk_count,
                name: _,
                md5: _,
            } => self.on_meta(xfer_id, file_size, chunk_count),
            FileMsg::Data {
                xfer_id,
                chunk_idx,
                data,
            } => self.on_data(xfer_id, chunk_idx, &data),
        }
    }

    /// CRC 坏帧 / 非测试帧：原文追加到旁路文件，本轮继续。
    pub fn note_bad_frame(&mut self, why: &str, raw: &[u8]) -> io::Result<FeedResult> {
        let Some(cur) = self.current.as_mut() else {
            return Ok(FeedResult::Nothing);
        };
        cur.last_activity = Instant::now();
        append_bad(cur, raw)?;
        let first = cur.first_error.is_none();
        if first {
            cur.first_error = Some(why.to_string());
            cur.first_got_idx = first_missing(&cur.received).unwrap_or(cur.chunk_count);
        }
        if first {
            Ok(FeedResult::Note(snapshot_note(cur)))
        } else {
            Ok(FeedResult::Nothing)
        }
    }

    pub fn abort_idle(&mut self) -> io::Result<Option<RecvOutcome>> {
        if self.current.is_none() {
            return Ok(None);
        }
        Ok(Some(self.fail_incomplete("收片超时")?))
    }

    /// 进程退出时丢掉未完成传输：不记失败、不保留 `fail-xfer*`、不产生 ACK。
    pub fn discard_inflight(&mut self) -> io::Result<Option<u32>> {
        let Some(cur) = self.current.take() else {
            return Ok(None);
        };
        let id = cur.xfer_id;
        let path = cur.path.clone();
        let bad = cur.bad_path.clone();
        drop(cur.file);
        drop(cur.bad_file);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bad);
        Ok(Some(id))
    }

    fn on_meta(
        &mut self,
        xfer_id: u32,
        file_size: u64,
        chunk_count: u32,
    ) -> io::Result<FeedResult> {
        if file_size == 0 || chunk_count == 0 {
            return Ok(FeedResult::Nothing);
        }
        let aborted = if self.current.is_some() {
            Some(self.fail_incomplete("中途新 META")?)
        } else {
            None
        };
        let path = self.dir.join(format!("recv-{xfer_id}.part"));
        let bad_path = self.dir.join(format!("xfer{xfer_id}-bad.bin"));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bad_path);
        let file = File::create(&path)?;
        let n = chunk_count as usize;
        self.current = Some(InFlight {
            xfer_id,
            file_size,
            chunk_count,
            learned_chunk: None,
            received: vec![false; n],
            got_chunks: 0,
            written: 0,
            path,
            file,
            bad_path,
            bad_file: None,
            last_activity: Instant::now(),
            first_error: None,
            first_got_idx: 0,
        });
        Ok(match aborted {
            Some(out) => FeedResult::AbortedPrevious(out),
            None => FeedResult::Nothing,
        })
    }

    fn on_data(&mut self, xfer_id: u32, chunk_idx: u32, data: &[u8]) -> io::Result<FeedResult> {
        let Some(cur) = self.current.as_ref() else {
            return Ok(FeedResult::Nothing);
        };
        if cur.xfer_id != xfer_id {
            return Ok(FeedResult::Nothing);
        }

        let expect = first_missing(&cur.received).unwrap_or(cur.chunk_count);
        let already = chunk_idx < cur.chunk_count && cur.received[chunk_idx as usize];
        let had_error = cur.first_error.is_some();
        let is_last = chunk_idx + 1 == cur.chunk_count;

        if already {
            let cur = self.current.as_mut().unwrap();
            cur.last_activity = Instant::now();
            let first = mark_error(cur, "重复片", chunk_idx);
            return Ok(if first && !had_error {
                FeedResult::Note(snapshot_note(cur))
            } else {
                FeedResult::Nothing
            });
        }

        let cur = self.current.as_mut().unwrap();
        if !is_last && chunk_idx < cur.chunk_count {
            if let Some(learned) = cur.learned_chunk {
                if data.len() as u32 != learned {
                    mark_error(cur, "片长不符", chunk_idx);
                }
            } else if !data.is_empty() {
                cur.learned_chunk = Some(data.len() as u32);
            }
        }
        if chunk_idx >= cur.chunk_count {
            mark_error(cur, "片号超出", chunk_idx);
        } else if chunk_idx != expect {
            mark_error(cur, "缺片", chunk_idx);
        }

        let offset = chunk_offset(cur, chunk_idx, data.len());
        cur.file.seek(SeekFrom::Start(offset))?;
        cur.file.write_all(data)?;
        cur.written = cur.written.saturating_add(data.len() as u64);
        cur.last_activity = Instant::now();
        if chunk_idx < cur.chunk_count && !cur.received[chunk_idx as usize] {
            cur.received[chunk_idx as usize] = true;
            cur.got_chunks += 1;
        }

        if cur.got_chunks == cur.chunk_count {
            return Ok(FeedResult::Done(self.finish_complete()?));
        }
        let first_note = !had_error && cur.first_error.is_some();
        if first_note {
            Ok(FeedResult::Note(snapshot_note(
                self.current.as_ref().unwrap(),
            )))
        } else {
            Ok(FeedResult::Nothing)
        }
    }

    fn finish_complete(&mut self) -> io::Result<RecvOutcome> {
        let mut cur = self.current.take().expect("finish 需要进行中的传输");
        drop(cur.bad_file.take());
        persist_and_drop_cache(&cur.file)?;
        drop(cur.file);
        let got = md5_file(&cur.path)?;
        let size_ok = cur.written == cur.file_size;
        if !size_ok && cur.first_error.is_none() {
            cur.first_error = Some("收齐但长度不符".into());
        }
        let mut recorded_golden = false;
        let expect = if let Some(preset) = self.expect_md5 {
            Some(preset)
        } else if let Some(golden) = self.golden {
            Some(golden)
        } else if size_ok {
            self.golden = Some(got);
            recorded_golden = true;
            Some(got)
        } else {
            None
        };
        let passed = size_ok && expect == Some(got);
        if recorded_golden && !passed {
            self.golden = None;
            recorded_golden = false;
        }
        let gap = first_missing(&cur.received);
        let kept_bad = finalize_bad(&cur.bad_path, cur.xfer_id, passed)?;
        let kept = if passed && recorded_golden {
            Some(keep_first(&self.dir, cur.xfer_id, &cur.path)?)
        } else if passed {
            let _ = fs::remove_file(&cur.path);
            None
        } else {
            Some(keep_fail(&self.dir, cur.xfer_id, &got, &cur.path)?)
        };
        let out = RecvOutcome {
            xfer_id: cur.xfer_id,
            passed,
            recorded_golden,
            got_md5: got,
            expect_md5: expect,
            kept_path: kept,
            kept_bad,
            why: if passed {
                None
            } else {
                cur.first_error.or_else(|| Some("MD5不匹配".into()))
            },
            written: cur.written,
            file_size: cur.file_size,
            got_chunks: cur.got_chunks,
            chunk_count: cur.chunk_count,
            first_gap: gap,
        };
        if out.passed {
            Ok(out)
        } else {
            finalize_fail_keep(&self.dir, self.max_fail_keep, out)
        }
    }

    fn fail_incomplete(&mut self, why: &str) -> io::Result<RecvOutcome> {
        let mut cur = self.current.take().expect("fail 需要进行中的传输");
        drop(cur.bad_file.take());
        let _ = persist_and_drop_cache(&cur.file);
        drop(cur.file);
        let got = md5_file(&cur.path).unwrap_or([0u8; MD5_LEN]);
        let gap = first_missing(&cur.received);
        let detail = match &cur.first_error {
            Some(e) if e != why => format!("{why} ({e})"),
            _ => why.to_string(),
        };
        let kept_bad = finalize_bad(&cur.bad_path, cur.xfer_id, false)?;
        let kept = keep_fail(&self.dir, cur.xfer_id, &got, &cur.path)?;
        finalize_fail_keep(
            &self.dir,
            self.max_fail_keep,
            RecvOutcome {
                xfer_id: cur.xfer_id,
                passed: false,
                recorded_golden: false,
                got_md5: got,
                expect_md5: self.expect_md5.or(self.golden),
                kept_path: Some(kept),
                kept_bad,
                why: Some(detail),
                written: cur.written,
                file_size: cur.file_size,
                got_chunks: cur.got_chunks,
                chunk_count: cur.chunk_count,
                first_gap: gap,
            },
        )
    }
}

/// 先 fsync 到设备，再建议内核丢掉本文件页缓存，随后由调用方关闭并重开回读。
/// `posix_fadvise` 失败不视为错误（tmpfs 等可能不支持），回读仍可能命中页缓存。
fn persist_and_drop_cache(file: &File) -> io::Result<()> {
    file.sync_all()?;
    let _ = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    Ok(())
}

fn first_missing(received: &[bool]) -> Option<u32> {
    received.iter().position(|got| !*got).map(|i| i as u32)
}

fn mark_error(cur: &mut InFlight, why: &str, got_idx: u32) -> bool {
    if cur.first_error.is_some() {
        return false;
    }
    cur.first_error = Some(why.to_string());
    cur.first_got_idx = got_idx;
    true
}

fn snapshot_note(cur: &InFlight) -> RecvNote {
    RecvNote {
        xfer_id: cur.xfer_id,
        why: cur
            .first_error
            .clone()
            .unwrap_or_else(|| "-".to_string()),
        expected_idx: first_missing(&cur.received).unwrap_or(cur.chunk_count),
        got_idx: cur.first_got_idx,
        written: cur.written,
        file_size: cur.file_size,
        missing: cur.received.iter().filter(|got| !*got).count() as u32,
    }
}

fn chunk_offset(cur: &InFlight, chunk_idx: u32, data_len: usize) -> u64 {
    let is_last = chunk_idx + 1 == cur.chunk_count;
    if let Some(c) = cur.learned_chunk {
        chunk_idx as u64 * u64::from(c)
    } else if is_last && (data_len as u64) <= cur.file_size {
        cur.file_size - data_len as u64
    } else {
        0
    }
}

fn append_bad(cur: &mut InFlight, raw: &[u8]) -> io::Result<()> {
    if cur.bad_file.is_none() {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&cur.bad_path)?;
        cur.bad_file = Some(file);
    }
    cur.bad_file.as_mut().unwrap().write_all(raw)?;
    Ok(())
}

fn finalize_bad(bad_path: &Path, xfer_id: u32, passed: bool) -> io::Result<Option<PathBuf>> {
    if !bad_path.exists() {
        return Ok(None);
    }
    if passed {
        let _ = fs::remove_file(bad_path);
        return Ok(None);
    }
    let dest = bad_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("fail-xfer{xfer_id}-bad.bin"));
    if dest.exists() {
        let _ = fs::remove_file(&dest);
    }
    fs::rename(bad_path, &dest)?;
    Ok(Some(dest))
}

fn keep_first(dir: &Path, xfer_id: u32, src: &Path) -> io::Result<PathBuf> {
    let dest = dir.join(format!("first-xfer{xfer_id}.bin"));
    if dest.exists() {
        let _ = fs::remove_file(&dest);
    }
    fs::rename(src, &dest)?;
    Ok(dest)
}

fn keep_fail(
    dir: &Path,
    xfer_id: u32,
    got: &[u8; MD5_LEN],
    src: &Path,
) -> io::Result<PathBuf> {
    let prefix: String = md5_hex(got).chars().take(8).collect();
    let dest = dir.join(format!("fail-xfer{xfer_id}-got{prefix}.bin"));
    if dest.exists() {
        let _ = fs::remove_file(&dest);
    }
    if src.exists() {
        fs::rename(src, &dest)?;
    } else {
        File::create(&dest)?;
    }
    Ok(dest)
}

fn write_fail_log(bin_path: &Path, body: &str) -> io::Result<PathBuf> {
    let log_path = bin_path.with_extension("log");
    let mut file = File::create(&log_path)?;
    writeln!(file, "{body}")?;
    Ok(log_path)
}

fn finalize_fail_keep(
    dir: &Path,
    max_fail_keep: usize,
    out: RecvOutcome,
) -> io::Result<RecvOutcome> {
    if let Some(kept) = out.kept_path.as_ref() {
        write_fail_log(kept, &out.log_line())?;
    }
    prune_fail_keep(dir, max_fail_keep)?;
    Ok(out)
}

/// `fail-xfer{id}-got{md5前8位}.bin` → xfer_id。其它 fail-* 不计名额。
fn parse_fail_got_bin(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("fail-xfer")?;
    let (id, suffix) = rest.split_once("-got")?;
    if suffix.ends_with(".bin") && suffix.len() > 4 {
        id.parse().ok()
    } else {
        None
    }
}

fn remove_fail_round(dir: &Path, got_bin: &Path, xfer_id: u32) {
    let log = got_bin.with_extension("log");
    let bad = dir.join(format!("fail-xfer{xfer_id}-bad.bin"));
    let _ = fs::remove_file(got_bin);
    let _ = fs::remove_file(log);
    let _ = fs::remove_file(bad);
}

fn prune_fail_keep(dir: &Path, max_keep: usize) -> io::Result<()> {
    let mut samples: Vec<(SystemTime, PathBuf, u32)> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            let path = ent.path();
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if let Some(xfer_id) = parse_fail_got_bin(&name) {
                let mtime = ent
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                samples.push((mtime, path, xfer_id));
            }
        }
    }
    samples.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let extra = samples.len().saturating_sub(max_keep);
    for (_, path, xfer_id) in samples.into_iter().take(extra) {
        remove_fail_round(dir, &path, xfer_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rs485-filexfer-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn meta(xfer_id: u32, data: &[u8], chunk: usize) -> FileMsg {
        let n = chunk_count(data.len() as u64, chunk).unwrap();
        FileMsg::Meta {
            xfer_id,
            file_size: data.len() as u64,
            chunk_count: n,
            md5: md5_bytes(data),
            name: "t.bin".into(),
        }
    }

    fn feed_file(engine: &mut RecvEngine, xfer_id: u32, data: &[u8], chunk: usize) -> RecvOutcome {
        match engine.feed(meta(xfer_id, data, chunk)).unwrap() {
            FeedResult::Nothing => {}
            other => panic!("unexpected meta: {other:?}"),
        }
        let chunks: Vec<&[u8]> = data.chunks(chunk).collect();
        let mut last = None;
        for (i, c) in chunks.iter().enumerate() {
            match engine
                .feed(FileMsg::Data {
                    xfer_id,
                    chunk_idx: i as u32,
                    data: c.to_vec(),
                })
                .unwrap()
            {
                FeedResult::Done(o) => last = Some(o),
                FeedResult::Nothing => {}
                other => panic!("unexpected data: {other:?}"),
            }
        }
        last.expect("应在最后一片完成")
    }

    fn assert_fail_log(out: &RecvOutcome) {
        let kept = out.kept_path.as_ref().expect("失败应保留样本");
        let log = kept.with_extension("log");
        assert!(log.exists(), "应有同名 .log {}", log.display());
        let body = fs::read_to_string(&log).unwrap();
        assert_eq!(body, format!("{}\n", out.log_line()));
        assert!(body.contains("原因="));
    }

    fn fail_got_names(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| parse_fail_got_bin(n).is_some())
            .collect()
    }

    #[test]
    fn msg_roundtrip() {
        let md5 = md5_bytes(b"hello");
        let meta = FileMsg::Meta {
            xfer_id: 3,
            file_size: 5,
            chunk_count: 1,
            md5,
            name: "hello.bin".into(),
        };
        let raw = encode_msg(&meta).unwrap();
        assert_eq!(decode_msg(&raw).unwrap(), meta);

        let data = FileMsg::Data {
            xfer_id: 3,
            chunk_idx: 0,
            data: b"hello".to_vec(),
        };
        let raw = encode_msg(&data).unwrap();
        assert_eq!(decode_msg(&raw).unwrap(), data);

        let ack = FileMsg::Ack {
            xfer_id: 3,
            result: ACK_OK,
            got_md5: md5,
        };
        let raw = encode_msg(&ack).unwrap();
        assert_eq!(decode_msg(&raw).unwrap(), ack);
    }

    #[test]
    fn wire_parse_via_test_frame() {
        let msg = FileMsg::Ack {
            xfer_id: 9,
            result: ACK_FAIL,
            got_md5: [0xab; 16],
        };
        let wire = encode_wire(1, &msg).unwrap();
        let parsed = parse_msgs(&wire);
        assert_eq!(parsed, vec![msg]);
    }

    #[test]
    fn parse_md5_hex_ok() {
        let h = md5_bytes(b"abc");
        assert_eq!(parse_md5_hex(&md5_hex(&h)).unwrap(), h);
        assert!(parse_md5_hex("zz").is_err());
    }

    #[test]
    fn discard_inflight_does_not_keep_or_fail() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 16).unwrap();
        let data = vec![1u8, 2, 3, 4];
        eng.feed(meta(0, &data, 2)).unwrap();
        eng.feed(FileMsg::Data {
            xfer_id: 0,
            chunk_idx: 0,
            data: vec![1, 2],
        })
        .unwrap();
        assert!(eng.inflight());
        assert_eq!(eng.discard_inflight().unwrap(), Some(0));
        assert!(!eng.inflight());
        assert!(eng.golden().is_none());
        assert!(!dir.join("recv-0.part").exists());
        let leftover: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(leftover.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn incomplete_does_not_record_golden() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 16).unwrap();
        let data = vec![1u8, 2, 3, 4];
        eng.feed(meta(0, &data, 2)).unwrap();
        eng.feed(FileMsg::Data {
            xfer_id: 0,
            chunk_idx: 0,
            data: vec![1, 2],
        })
        .unwrap();
        let out = eng.abort_idle().unwrap().unwrap();
        assert!(!out.passed);
        assert!(!out.recorded_golden);
        assert!(eng.golden().is_none());
        let kept = out.kept_path.clone().unwrap();
        assert!(kept.exists());
        assert_fail_log(&out);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_complete_records_golden_and_keeps() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 16).unwrap();
        let data = b"payload-one".to_vec();
        let out = feed_file(&mut eng, 1, &data, 4);
        assert!(out.passed);
        assert!(out.recorded_golden);
        assert_eq!(eng.golden(), Some(md5_bytes(&data)));
        assert!(!dir.join("recv-1.part").exists());
        let kept = dir.join("first-xfer1.bin");
        assert_eq!(out.kept_path.as_ref(), Some(&kept));
        assert_eq!(fs::read(&kept).unwrap(), data);
        assert!(!kept.with_extension("log").exists());
        assert!(fail_got_names(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_match_deletes_mismatch_keeps() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 16).unwrap();
        let data = b"same-bytes-ok".to_vec();
        let first = feed_file(&mut eng, 0, &data, 5);
        assert!(first.recorded_golden);
        assert!(dir.join("first-xfer0.bin").exists());
        let second = feed_file(&mut eng, 1, &data, 5);
        assert!(second.passed);
        assert!(!second.recorded_golden);
        assert!(second.kept_path.is_none());
        assert!(!dir.join("recv-1.part").exists());
        assert!(dir.join("first-xfer0.bin").exists());
        assert!(!dir.join("recv-1.log").exists());

        let other = b"DIFFERENT!!!!".to_vec();
        let third = feed_file(&mut eng, 2, &other, 5);
        assert!(!third.passed);
        let kept = third.kept_path.clone().unwrap();
        assert!(kept.exists());
        assert_eq!(fs::read(&kept).unwrap(), other);
        assert_fail_log(&third);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expect_md5_rejects_first_mismatch() {
        let dir = temp_dir();
        let want = md5_bytes(b"wanted");
        let mut eng = RecvEngine::new(dir.clone(), Some(want), 16).unwrap();
        let data = b"other!!".to_vec();
        let out = feed_file(&mut eng, 0, &data, 8);
        assert!(!out.passed);
        assert!(!out.recorded_golden);
        assert!(eng.golden().is_none());
        let kept = out.kept_path.clone().unwrap();
        assert!(kept.exists());
        assert_fail_log(&out);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_then_reread_md5() {
        let dir = temp_dir();
        let path = dir.join("cold.bin");
        let data = b"cold-read-payload";
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(data).unwrap();
            persist_and_drop_cache(&f).unwrap();
        }
        assert_eq!(md5_file(&path).unwrap(), md5_bytes(data));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gap_chunk_continues_without_golden() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 16).unwrap();
        let data = vec![9u8; 6];
        eng.feed(meta(7, &data, 2)).unwrap();
        match eng
            .feed(FileMsg::Data {
                xfer_id: 7,
                chunk_idx: 1,
                data: vec![9, 9],
            })
            .unwrap()
        {
            FeedResult::Note(note) => {
                assert_eq!(note.why, "缺片");
                assert!(eng.inflight());
                assert!(eng.golden().is_none());
            }
            other => panic!("{other:?}"),
        }
        match eng
            .feed(FileMsg::Data {
                xfer_id: 7,
                chunk_idx: 2,
                data: vec![9, 9],
            })
            .unwrap()
        {
            FeedResult::Nothing => {}
            other => panic!("后续片应继续: {other:?}"),
        }
        let part = dir.join("recv-7.part");
        assert_eq!(part.metadata().unwrap().len(), 6);
        let raw = fs::read(&part).unwrap();
        assert_eq!(&raw[2..], &[9, 9, 9, 9]);
        let out = eng.abort_idle().unwrap().unwrap();
        assert!(!out.passed);
        assert!(eng.golden().is_none());
        assert_eq!(out.written, 4);
        assert_eq!(out.first_gap, Some(0));
        let kept = out.kept_path.clone().unwrap();
        assert!(kept.exists());
        assert_eq!(kept.metadata().unwrap().len(), 6);
        assert_fail_log(&out);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_order_fills_and_passes() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 16).unwrap();
        let data = b"abcdef".to_vec();
        eng.feed(meta(0, &data, 2)).unwrap();
        match eng
            .feed(FileMsg::Data {
                xfer_id: 0,
                chunk_idx: 1,
                data: b"cd".to_vec(),
            })
            .unwrap()
        {
            FeedResult::Note(_) => {}
            other => panic!("{other:?}"),
        }
        match eng
            .feed(FileMsg::Data {
                xfer_id: 0,
                chunk_idx: 2,
                data: b"ef".to_vec(),
            })
            .unwrap()
        {
            FeedResult::Nothing => {}
            other => panic!("{other:?}"),
        }
        match eng
            .feed(FileMsg::Data {
                xfer_id: 0,
                chunk_idx: 0,
                data: b"ab".to_vec(),
            })
            .unwrap()
        {
            FeedResult::Done(out) => {
                assert!(out.passed);
                assert!(out.recorded_golden);
                let kept = out.kept_path.unwrap();
                assert_eq!(fs::read(&kept).unwrap(), data);
                assert!(!kept.with_extension("log").exists());
            }
            other => panic!("{other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_keeps_first_write() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 16).unwrap();
        let data = vec![1u8, 2, 3, 4];
        eng.feed(meta(0, &data, 2)).unwrap();
        eng.feed(FileMsg::Data {
            xfer_id: 0,
            chunk_idx: 0,
            data: vec![1, 2],
        })
        .unwrap();
        match eng
            .feed(FileMsg::Data {
                xfer_id: 0,
                chunk_idx: 0,
                data: vec![9, 9],
            })
            .unwrap()
        {
            FeedResult::Note(note) => assert_eq!(note.why, "重复片"),
            other => panic!("{other:?}"),
        }
        match eng
            .feed(FileMsg::Data {
                xfer_id: 0,
                chunk_idx: 1,
                data: vec![3, 4],
            })
            .unwrap()
        {
            FeedResult::Done(out) => {
                assert!(out.passed);
                let kept = out.kept_path.unwrap();
                assert_eq!(fs::read(&kept).unwrap(), data);
                assert!(!kept.with_extension("log").exists());
            }
            other => panic!("{other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_frame_saved_on_fail_deleted_on_pass() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 16).unwrap();
        let data = vec![1u8, 2, 3, 4];
        eng.feed(meta(3, &data, 2)).unwrap();
        match eng.note_bad_frame("CRC错误", &[0xff, 0x00]).unwrap() {
            FeedResult::Note(note) => assert_eq!(note.why, "CRC错误"),
            other => panic!("{other:?}"),
        }
        eng.feed(FileMsg::Data {
            xfer_id: 3,
            chunk_idx: 0,
            data: vec![1, 2],
        })
        .unwrap();
        let out = eng.abort_idle().unwrap().unwrap();
        assert!(!out.passed);
        let bad = out.kept_bad.clone().expect("应保留坏帧");
        assert_eq!(fs::read(&bad).unwrap(), [0xff, 0x00]);
        assert!(bad
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("fail-xfer3-bad"));
        assert_fail_log(&out);
        assert!(out.log_line().contains("坏帧="));
        let _ = fs::remove_dir_all(&dir);

        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 16).unwrap();
        eng.feed(meta(4, &data, 2)).unwrap();
        eng.note_bad_frame("CRC错误", &[0xaa]).unwrap();
        let out = feed_remaining(&mut eng, 4, &data, 2);
        assert!(out.passed);
        assert!(out.kept_bad.is_none());
        assert!(!dir.join("xfer4-bad.bin").exists());
        assert!(!dir.join("fail-xfer4-bad.bin").exists());
        assert!(fail_got_names(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    fn feed_remaining(
        engine: &mut RecvEngine,
        xfer_id: u32,
        data: &[u8],
        chunk: usize,
    ) -> RecvOutcome {
        let chunks: Vec<&[u8]> = data.chunks(chunk).collect();
        let mut last = None;
        for (i, c) in chunks.iter().enumerate() {
            match engine
                .feed(FileMsg::Data {
                    xfer_id,
                    chunk_idx: i as u32,
                    data: c.to_vec(),
                })
                .unwrap()
            {
                FeedResult::Done(o) => last = Some(o),
                FeedResult::Nothing | FeedResult::Note(_) => {}
                other => panic!("unexpected: {other:?}"),
            }
        }
        last.expect("应完成")
    }

    #[test]
    fn prune_fail_does_not_delete_first() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 2).unwrap();
        let data = b"keep-first!!".to_vec();
        let first = feed_file(&mut eng, 0, &data, 4);
        assert!(first.recorded_golden);
        assert!(dir.join("first-xfer0.bin").exists());
        for i in 1..=4u32 {
            let other = format!("fail-payload-{i}!!").into_bytes();
            let out = feed_file(&mut eng, i, &other, 4);
            assert!(!out.passed);
        }
        assert!(dir.join("first-xfer0.bin").exists());
        let got = fail_got_names(&dir);
        assert_eq!(got.len(), 2);
        for name in &got {
            let bin = dir.join(name);
            assert!(bin.with_extension("log").exists(), "{name} 应有同名 .log");
        }
        let logs: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("fail-") && n.ends_with(".log"))
            .collect();
        assert_eq!(logs.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_fail_removes_log_and_bad_together() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 1).unwrap();
        let data = b"keep-first!!".to_vec();
        let first = feed_file(&mut eng, 0, &data, 4);
        assert!(first.recorded_golden);

        let other = vec![1u8, 2, 3, 4];
        eng.feed(meta(1, &other, 2)).unwrap();
        eng.note_bad_frame("CRC错误", &[0xff]).unwrap();
        let old = eng.abort_idle().unwrap().unwrap();
        assert!(!old.passed);
        let old_bin = old.kept_path.clone().unwrap();
        let old_log = old_bin.with_extension("log");
        let old_bad = old.kept_bad.clone().unwrap();
        assert!(old_bin.exists());
        assert!(old_log.exists());
        assert!(old_bad.exists());

        let newer = b"fail-payload-2!!".to_vec();
        let kept = feed_file(&mut eng, 2, &newer, 4);
        assert!(!kept.passed);
        assert_fail_log(&kept);
        assert!(!old_bin.exists());
        assert!(!old_log.exists());
        assert!(!old_bad.exists());
        assert_eq!(fail_got_names(&dir).len(), 1);
        assert!(dir.join("first-xfer0.bin").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_fail_got_bin_only_matches_sample() {
        assert_eq!(parse_fail_got_bin("fail-xfer3-gota1b2c3d4.bin"), Some(3));
        assert_eq!(parse_fail_got_bin("fail-xfer10-gotabcdef01.bin"), Some(10));
        assert_eq!(parse_fail_got_bin("fail-xfer3-gota1b2c3d4.log"), None);
        assert_eq!(parse_fail_got_bin("fail-xfer3-bad.bin"), None);
        assert_eq!(parse_fail_got_bin("first-xfer3.bin"), None);
    }
}
