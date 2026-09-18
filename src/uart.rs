use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serialport::{DataBits, FlowControl, Parity, StopBits, TTYPort};

use crate::gpio::Gpio;

#[derive(Clone, Copy, Debug)]
pub struct UartFormat {
    pub parity: Parity,
    pub data_bits: DataBits,
    pub flow: FlowControl,
    pub stop_bits: StopBits,
}

impl UartFormat {
    /// 解析手册 uarttest 风格：`N8N1` 或 `115200N8N1`。
    /// parity: N/O/E；data: 5-8；flow: Y/N；stop: 1/2。
    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim().to_ascii_uppercase();
        let rest: String = s.chars().skip_while(|c| c.is_ascii_digit()).collect();
        let bytes = rest.as_bytes();
        anyhow::ensure!(
            bytes.len() == 4,
            "串口格式应为 N8N1 或 115200N8N1，实际: {raw}"
        );

        let parity = match bytes[0] {
            b'N' => Parity::None,
            b'O' => Parity::Odd,
            b'E' => Parity::Even,
            b'S' => {
                anyhow::bail!("space/stick 校验(S) 暂不支持，请使用 N/O/E")
            }
            other => anyhow::bail!("未知校验位 '{}'", other as char),
        };

        let data_bits = match bytes[1] {
            b'5' => DataBits::Five,
            b'6' => DataBits::Six,
            b'7' => DataBits::Seven,
            b'8' => DataBits::Eight,
            other => anyhow::bail!("数据位仅支持 5/6/7/8，实际 '{}'", other as char),
        };

        let flow = match bytes[2] {
            b'N' => FlowControl::None,
            b'Y' => FlowControl::Hardware,
            other => anyhow::bail!("流控仅支持 Y/N，实际 '{}'", other as char),
        };

        let stop_bits = match bytes[3] {
            b'1' => StopBits::One,
            b'2' => StopBits::Two,
            other => anyhow::bail!("停止位仅支持 1/2，实际 '{}'", other as char),
        };

        Ok(Self {
            parity,
            data_bits,
            flow,
            stop_bits,
        })
    }
}

pub fn open_port(
    device: &str,
    baud: u32,
    format: UartFormat,
    timeout: Duration,
) -> Result<TTYPort> {
    let builder = serialport::new(device, baud)
        .data_bits(format.data_bits)
        .parity(format.parity)
        .stop_bits(format.stop_bits)
        .flow_control(format.flow)
        .timeout(timeout);
    TTYPort::open(&builder).with_context(|| format!("打开串口 {device} 失败"))
}

pub fn send_frame(
    port: &mut TTYPort,
    gpio: &Gpio,
    data: &[u8],
    tx_setup: Duration,
    tx_hold: Duration,
) -> Result<()> {
    gpio.set_tx()?;
    thread::sleep(tx_setup);
    port.write_all(data)
        .context("RS485 写入失败")?;
    port.flush().context("RS485 flush 失败")?;
    drain_port(port);
    if !tx_hold.is_zero() {
        thread::sleep(tx_hold);
    }
    gpio.set_rx()?;
    Ok(())
}

/// 把内核接收缓冲里已经到达的字节抽进 `dest`，不等待后续数据。
/// `FIONREAD` 失败时返回 0，不阻塞。
pub fn drain_available(port: &mut TTYPort, dest: &mut [u8]) -> io::Result<usize> {
    if dest.is_empty() {
        return Ok(0);
    }
    let fd = port.as_raw_fd();
    let mut avail: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut avail) };
    if rc < 0 || avail <= 0 {
        return Ok(0);
    }
    let want = (avail as usize).min(dest.len());
    let mut got = 0usize;
    while got < want {
        match port.read(&mut dest[got..want]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err)
                if err.kind() == io::ErrorKind::TimedOut
                    || err.kind() == io::ErrorKind::WouldBlock =>
            {
                break;
            }
            Err(err) => return Err(err),
        }
    }
    Ok(got)
}

pub fn read_frame(port: &mut TTYPort, max_len: usize) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    let mut tmp = [0u8; 256];
    loop {
        match port.read(&mut tmp) {
            Ok(0) => {
                if frame.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Ok(n) => {
                let room = max_len.saturating_sub(frame.len());
                frame.extend_from_slice(&tmp[..n.min(room)]);
                if frame.len() >= max_len {
                    break;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                if frame.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if frame.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(Some(frame))
}

fn drain_port(port: &TTYPort) {
    let fd = port.as_raw_fd();
    let rc = unsafe { libc::tcdrain(fd) };
    if rc != 0 {
        thread::sleep(Duration::from_millis(2));
    }
}

/// 相对打开串口后的 UART 线路错误计数（TIOCGICOUNT 差值）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UartCounters {
    pub frame: u64,
    pub overrun: u64,
    pub parity: u64,
    pub brk: u64,
    pub buf_overrun: u64,
}

impl UartCounters {
    pub fn error_total(self) -> u64 {
        self.frame
            .saturating_add(self.overrun)
            .saturating_add(self.parity)
            .saturating_add(self.brk)
            .saturating_add(self.buf_overrun)
    }

    fn saturating_delta(self, baseline: Self) -> Self {
        Self {
            frame: self.frame.saturating_sub(baseline.frame),
            overrun: self.overrun.saturating_sub(baseline.overrun),
            parity: self.parity.saturating_sub(baseline.parity),
            brk: self.brk.saturating_sub(baseline.brk),
            buf_overrun: self.buf_overrun.saturating_sub(baseline.buf_overrun),
        }
    }
}

#[repr(C)]
struct SerialIcounter {
    cts: i32,
    dsr: i32,
    rng: i32,
    dcd: i32,
    rx: i32,
    tx: i32,
    frame: i32,
    overrun: i32,
    parity: i32,
    brk: i32,
    buf_overrun: i32,
    reserved: [i32; 9],
}

fn as_cnt(v: i32) -> u64 {
    v.max(0) as u64
}

pub fn read_icount(fd: i32) -> Option<UartCounters> {
    let mut ic = unsafe { std::mem::zeroed::<SerialIcounter>() };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGICOUNT, &mut ic) };
    if rc < 0 {
        None
    } else {
        Some(UartCounters {
            frame: as_cnt(ic.frame),
            overrun: as_cnt(ic.overrun),
            parity: as_cnt(ic.parity),
            brk: as_cnt(ic.brk),
            buf_overrun: as_cnt(ic.buf_overrun),
        })
    }
}

pub struct IcountWatch {
    fd: i32,
    baseline: Option<UartCounters>,
}

impl IcountWatch {
    pub fn new(port: &TTYPort) -> Self {
        let fd = port.as_raw_fd();
        Self {
            fd,
            baseline: read_icount(fd),
        }
    }

    pub fn delta(&self) -> Option<UartCounters> {
        let now = read_icount(self.fd)?;
        Some(now.saturating_delta(self.baseline?))
    }
}
