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

    pub fn bits_per_byte(self) -> u32 {
        let data = match self.data_bits {
            DataBits::Five => 5,
            DataBits::Six => 6,
            DataBits::Seven => 7,
            DataBits::Eight => 8,
        };
        let parity = match self.parity {
            Parity::None => 0,
            _ => 1,
        };
        let stop = match self.stop_bits {
            StopBits::One => 1,
            StopBits::Two => 2,
        };
        1 + data + parity + stop
    }
}

pub fn default_tx_hold_us(baud: u32, format: UartFormat) -> u64 {
    let baud = baud.max(1) as u64;
    let byte_us = (format.bits_per_byte() as u64 * 1_000_000).div_ceil(baud);
    byte_us.saturating_mul(2).max(50)
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
    thread::sleep(tx_hold);
    gpio.set_rx()?;
    Ok(())
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
