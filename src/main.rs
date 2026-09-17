mod gpio;
mod uart;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serialport::TTYPort;

use crate::gpio::Gpio;
use crate::uart::{default_tx_hold_us, open_port, read_frame, send_frame, UartFormat};

#[derive(Parser, Debug)]
#[command(
    name = "rs485-test",
    about = "QCM6125 RS485 半双工测试：GPIO 拉高收、拉低发，默认 /dev/ttyHS3 + GPIO123"
)]
struct Cli {
    /// 串口设备
    #[arg(short, long, default_value = "/dev/ttyHS3")]
    device: String,

    /// 波特率
    #[arg(short, long, default_value_t = 115200)]
    baud: u32,

    /// uarttest 风格格式：N8N1 或 115200N8N1
    #[arg(long, default_value = "N8N1")]
    format: String,

    /// 方向控制 GPIO 编号
    #[arg(short, long, default_value_t = 123)]
    gpio: u32,

    /// 拉低 TX 后、写串口前的等待（微秒）
    #[arg(long, default_value_t = 50)]
    tx_setup_us: u64,

    /// 写完并 tcdrain 后、拉高 RX 前的等待（微秒）。默认 2 个字节时间
    #[arg(long)]
    tx_hold_us: Option<u64>,

    /// 帧间隔空闲判定（毫秒）
    #[arg(long, default_value_t = 30)]
    frame_idle_ms: u64,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 默认：串口收包打印，stdin 每行发给 PC
    Duplex,
    /// 收到一帧后原样回发
    Echo,
    /// 发送一次文本或文件
    Send {
        text: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        /// 发完后继续接收
        #[arg(long)]
        stay: bool,
    },
    /// 只接收并打印
    Recv,
}

struct Runtime {
    port: TTYPort,
    gpio: Gpio,
    tx_setup: Duration,
    tx_hold: Duration,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("错误: {err:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let format = UartFormat::parse(&cli.format)?;
    let tx_hold_us = cli
        .tx_hold_us
        .unwrap_or_else(|| default_tx_hold_us(cli.baud, format));
    let timeout = Duration::from_millis(cli.frame_idle_ms.max(5));

    log_line(&format!(
        "cfg device={} baud={} format={} gpio={} tx_setup={}us tx_hold={}us frame_idle={}ms",
        cli.device,
        cli.baud,
        cli.format.to_ascii_uppercase(),
        cli.gpio,
        cli.tx_setup_us,
        tx_hold_us,
        timeout.as_millis()
    ));

    let gpio = Gpio::open(cli.gpio)?;
    let port = open_port(&cli.device, cli.baud, format, timeout)?;
    let mut rt = Runtime {
        port,
        gpio,
        tx_setup: Duration::from_micros(cli.tx_setup_us),
        tx_hold: Duration::from_micros(tx_hold_us),
    };

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        let gpio = rt.gpio.clone();
        ctrlc::set_handler(move || {
            let _ = gpio.set_rx();
            running.store(false, Ordering::SeqCst);
        })
        .context("安装信号处理失败")?;
    }

    match cli.command.unwrap_or(Command::Duplex) {
        Command::Duplex => run_duplex(rt, running)?,
        Command::Echo => run_echo(&mut rt, &running)?,
        Command::Send { text, file, stay } => {
            let payload = load_send_payload(text, file)?;
            run_send(&mut rt, &running, &payload, stay)?;
        }
        Command::Recv => run_recv(&mut rt, &running)?,
    }

    Ok(())
}

fn load_send_payload(text: Option<String>, file: Option<PathBuf>) -> Result<Vec<u8>> {
    match (text, file) {
        (Some(_), Some(_)) => anyhow::bail!("send 不能同时指定文本和 --file"),
        (None, None) => anyhow::bail!("send 需要文本或 --file"),
        (Some(text), None) => Ok(text.into_bytes()),
        (None, Some(path)) => std::fs::read(&path)
            .with_context(|| format!("读取 {} 失败", path.display())),
    }
}

fn run_duplex(rt: Runtime, running: Arc<AtomicBool>) -> Result<()> {
    log_line("mode=duplex  stdin 每行发送，串口收包打印。Ctrl-C 退出。");
    let port = Arc::new(Mutex::new(rt.port));
    let gpio = Arc::new(rt.gpio);
    let tx_setup = rt.tx_setup;
    let tx_hold = rt.tx_hold;

    let rx_port = port.clone();
    let rx_running = running.clone();
    let rx = std::thread::spawn(move || -> Result<()> {
        while rx_running.load(Ordering::SeqCst) {
            let mut guard = rx_port.lock().expect("串口锁损坏");
            match read_frame(&mut guard, 4096) {
                Ok(Some(frame)) => {
                    drop(guard);
                    log_bytes("RX", &frame);
                }
                Ok(None) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err).context("duplex 接收失败"),
            }
        }
        Ok(())
    });

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        let line = line.context("读取 stdin 失败")?;
        let mut data = line.into_bytes();
        data.push(b'\n');
        {
            let mut guard = port.lock().expect("串口锁损坏");
            send_frame(&mut guard, gpio.as_ref(), &data, tx_setup, tx_hold)?;
        }
        log_bytes("TX", &data);
    }

    if io::stdin().is_terminal() {
        running.store(false, Ordering::SeqCst);
    } else {
        log_line("stdin 已关闭，继续接收直到收到停止信号");
        while running.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    running.store(false, Ordering::SeqCst);
    rx.join().expect("接收线程 panic")?;
    Ok(())
}

fn run_echo(rt: &mut Runtime, running: &AtomicBool) -> Result<()> {
    log_line("mode=echo  收到一帧后原样回发。Ctrl-C 退出。");
    while running.load(Ordering::SeqCst) {
        match read_frame(&mut rt.port, 4096) {
            Ok(Some(frame)) => {
                log_bytes("RX", &frame);
                send_frame(
                    &mut rt.port,
                    &rt.gpio,
                    &frame,
                    rt.tx_setup,
                    rt.tx_hold,
                )?;
                log_bytes("TX", &frame);
            }
            Ok(None) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err).context("echo 接收失败"),
        }
    }
    Ok(())
}

fn run_send(
    rt: &mut Runtime,
    running: &AtomicBool,
    payload: &[u8],
    stay: bool,
) -> Result<()> {
    anyhow::ensure!(!payload.is_empty(), "发送内容为空");
    send_frame(
        &mut rt.port,
        &rt.gpio,
        payload,
        rt.tx_setup,
        rt.tx_hold,
    )?;
    log_bytes("TX", payload);
    if stay {
        log_line("mode=send --stay  发完后继续接收。Ctrl-C 退出。");
        run_recv(rt, running)?;
    }
    Ok(())
}

fn run_recv(rt: &mut Runtime, running: &AtomicBool) -> Result<()> {
    rt.gpio.set_rx()?;
    log_line("mode=recv  只接收。Ctrl-C 退出。");
    while running.load(Ordering::SeqCst) {
        match read_frame(&mut rt.port, 4096) {
            Ok(Some(frame)) => log_bytes("RX", &frame),
            Ok(None) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err).context("recv 接收失败"),
        }
    }
    Ok(())
}

fn log_bytes(dir: &str, data: &[u8]) {
    log_line(&format!(
        "[{dir}] len={} ascii=\"{}\" hex={}",
        data.len(),
        format_ascii(data),
        format_hex(data)
    ));
}

fn log_line(msg: &str) {
    let ts = now_millis();
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{ts} {msg}");
    let _ = out.flush();
}

fn now_millis() -> String {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) } != 0 {
        return "----/--/-- --:--:--.---".to_string();
    }
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&ts.tv_sec, &mut tm) }.is_null() {
        return "----/--/-- --:--:--.---".to_string();
    }
    let ms = (ts.tv_nsec / 1_000_000).clamp(0, 999);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        ms
    )
}

fn format_ascii(data: &[u8]) -> String {
    data.iter()
        .map(|&b| {
            if (0x20..=0x7e).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

fn format_hex(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
