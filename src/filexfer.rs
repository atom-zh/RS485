//! 文件传输：测试帧 payload 内的 META / DATA / ACK，以及接收侧 MD5 校验状态机。

use std::fs::{self, File};
use std::io::{self, Read, Write};
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

pub fn parse_msgs(raw: &[u8]) -> Vec<FileMsg> {
    inspect(raw)
        .into_iter()
        .filter_map(|view| match view {
            FrameView::Ok(frame) => decode_msg(&frame.payload).ok(),
            _ => None,
        })
        .collect()
}

#[derive(Debug)]
pub struct RecvOutcome {
    pub xfer_id: u32,
    pub passed: bool,
    pub recorded_golden: bool,
    pub got_md5: [u8; MD5_LEN],
    pub expect_md5: Option<[u8; MD5_LEN]>,
    pub kept_path: Option<PathBuf>,
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
            format!(
                "file xfer={} 已记录基准 MD5={} 通过 已删除",
                self.xfer_id, got
            )
        } else if self.passed {
            format!("file xfer={} 通过 md5={} 已删除", self.xfer_id, got)
        } else {
            let expect = self
                .expect_md5
                .map(|m| md5_hex(&m))
                .unwrap_or_else(|| "-".to_string());
            let kept = self
                .kept_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "未保留".to_string());
            format!(
                "file xfer={} 失败 expect={} got={} 已保留 {}",
                self.xfer_id, expect, got, kept
            )
        }
    }
}

#[derive(Debug)]
pub enum FeedResult {
    Nothing,
    Done(RecvOutcome),
    /// 新 META 打断未完成传输；新传输已开始。
    AbortedPrevious(RecvOutcome),
}

struct InFlight {
    xfer_id: u32,
    file_size: u64,
    chunk_count: u32,
    next_idx: u32,
    written: u64,
    path: PathBuf,
    file: File,
    last_activity: Instant,
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
        drop(cur.file);
        if cur.path.exists() {
            let _ = fs::remove_file(&cur.path);
        }
        Ok(Some(cur.xfer_id))
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
        let _ = fs::remove_file(&path);
        let file = File::create(&path)?;
        self.current = Some(InFlight {
            xfer_id,
            file_size,
            chunk_count,
            next_idx: 0,
            written: 0,
            path,
            file,
            last_activity: Instant::now(),
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
        if chunk_idx != cur.next_idx || chunk_idx >= cur.chunk_count {
            return Ok(FeedResult::Done(self.fail_incomplete("分片序号错误")?));
        }
        let add = data.len() as u64;
        if cur.written.saturating_add(add) > cur.file_size {
            return Ok(FeedResult::Done(self.fail_incomplete("分片超出文件长度")?));
        }
        let cur = self.current.as_mut().unwrap();
        cur.file.write_all(data)?;
        cur.written += add;
        cur.next_idx += 1;
        cur.last_activity = Instant::now();
        if cur.next_idx == cur.chunk_count {
            if cur.written != cur.file_size {
                return Ok(FeedResult::Done(self.fail_incomplete("收齐但长度不符")?));
            }
            return Ok(FeedResult::Done(self.finish_complete()?));
        }
        Ok(FeedResult::Nothing)
    }

    fn finish_complete(&mut self) -> io::Result<RecvOutcome> {
        let cur = self.current.take().expect("finish 需要进行中的传输");
        persist_and_drop_cache(&cur.file)?;
        drop(cur.file);
        let got = md5_file(&cur.path)?;
        let mut recorded_golden = false;
        let expect = if let Some(preset) = self.expect_md5 {
            Some(preset)
        } else if let Some(golden) = self.golden {
            Some(golden)
        } else {
            self.golden = Some(got);
            recorded_golden = true;
            Some(got)
        };
        let passed = expect == Some(got);
        let kept = if passed {
            let _ = fs::remove_file(&cur.path);
            None
        } else {
            Some(keep_fail(&self.dir, cur.xfer_id, &got, &cur.path, self.max_fail_keep)?)
        };
        Ok(RecvOutcome {
            xfer_id: cur.xfer_id,
            passed,
            recorded_golden,
            got_md5: got,
            expect_md5: expect,
            kept_path: kept,
        })
    }

    fn fail_incomplete(&mut self, _why: &str) -> io::Result<RecvOutcome> {
        let cur = self.current.take().expect("fail 需要进行中的传输");
        let _ = persist_and_drop_cache(&cur.file);
        drop(cur.file);
        let got = md5_file(&cur.path).unwrap_or([0u8; MD5_LEN]);
        let kept = keep_fail(&self.dir, cur.xfer_id, &got, &cur.path, self.max_fail_keep)?;
        Ok(RecvOutcome {
            xfer_id: cur.xfer_id,
            passed: false,
            recorded_golden: false,
            got_md5: got,
            expect_md5: self.expect_md5.or(self.golden),
            kept_path: Some(kept),
        })
    }
}

/// 先 fsync 到设备，再建议内核丢掉本文件页缓存，随后由调用方关闭并重开回读。
/// `posix_fadvise` 失败不视为错误（tmpfs 等可能不支持），回读仍可能命中页缓存。
fn persist_and_drop_cache(file: &File) -> io::Result<()> {
    file.sync_all()?;
    let _ = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    Ok(())
}

fn keep_fail(
    dir: &Path,
    xfer_id: u32,
    got: &[u8; MD5_LEN],
    src: &Path,
    max_fail_keep: usize,
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
    prune_fail_keep(dir, max_fail_keep)?;
    Ok(dest)
}

fn prune_fail_keep(dir: &Path, max_keep: usize) -> io::Result<()> {
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            let path = ent.path();
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("fail-") {
                let mtime = ent
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((mtime, path));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let extra = files.len().saturating_sub(max_keep);
    for (_, path) in files.into_iter().take(extra) {
        let _ = fs::remove_file(path);
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
        assert!(out.kept_path.unwrap().exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_complete_records_golden_and_deletes() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 16).unwrap();
        let data = b"payload-one".to_vec();
        let out = feed_file(&mut eng, 1, &data, 4);
        assert!(out.passed);
        assert!(out.recorded_golden);
        assert_eq!(eng.golden(), Some(md5_bytes(&data)));
        assert!(!dir.join("recv-1.part").exists());
        let leftover: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(leftover.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_match_deletes_mismatch_keeps() {
        let dir = temp_dir();
        let mut eng = RecvEngine::new(dir.clone(), None, 16).unwrap();
        let data = b"same-bytes-ok".to_vec();
        let first = feed_file(&mut eng, 0, &data, 5);
        assert!(first.recorded_golden);
        let second = feed_file(&mut eng, 1, &data, 5);
        assert!(second.passed);
        assert!(!second.recorded_golden);
        assert!(second.kept_path.is_none());

        let other = b"DIFFERENT!!!!".to_vec();
        let third = feed_file(&mut eng, 2, &other, 5);
        assert!(!third.passed);
        let kept = third.kept_path.unwrap();
        assert!(kept.exists());
        assert_eq!(fs::read(&kept).unwrap(), other);
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
        assert!(out.kept_path.unwrap().exists());
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
    fn gap_chunk_fails_without_golden() {
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
            FeedResult::Done(out) => {
                assert!(!out.passed);
                assert!(eng.golden().is_none());
            }
            other => panic!("{other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
