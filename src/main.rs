mod frame;
mod gpio;
mod uart;

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serialport::{SerialPort, TTYPort};

use crate::frame::{encode, inspect, pattern_payload, FrameView, SeqTracker};
use crate::gpio::Gpio;
use crate::uart::{drain_available, open_port, read_frame, send_frame, IcountWatch, UartFormat};

const MAX_IDLE_FRAME: usize = 8192;
const REVERSE_MAX: usize = 8;
const REVERSE_POLL: Duration = Duration::from_millis(10);
const REVERSE_IDLE_TX: Duration = Duration::from_secs(6);

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

    /// 写完并 tcdrain 后、拉高 RX 前的等待（微秒）。默认 0：发完立刻切接收
    #[arg(long, default_value_t = 0)]
    tx_hold_us: u64,

    /// 帧间隔空闲判定（毫秒）
    #[arg(long, default_value_t = 30)]
    frame_idle_ms: u64,

    /// 静默：不打印报文内容，只输出统计
    #[arg(short = 'q', long = "quiet", alias = "silent")]
    quiet: bool,

    /// 按测试帧（A55A+序号+CRC16）校验收包
    #[arg(long)]
    crc: bool,

    /// 静默统计打印间隔（毫秒）。0 表示每包打印。默认 1000
    #[arg(long, default_value_t = 1000)]
    stats_interval_ms: u64,

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
    /// 连续发送序号+CRC 测试帧
    Traffic {
        /// 发送帧数，0 表示直到 Ctrl-C
        #[arg(long, default_value_t = 0)]
        count: u64,
        /// payload 字节数
        #[arg(long, default_value_t = 32)]
        payload: usize,
        /// 帧间隔（毫秒）
        #[arg(long, default_value_t = 20)]
        interval_ms: u64,
    },
    /// 测试：首帧 1～8 字节倒序回发并缓存，之后只比对；6s 无接收则补发缓存
    #[command(visible_alias = "test")]
    Reverse,
}

struct ReverseCache {
    input: [u8; REVERSE_MAX],
    output: [u8; REVERSE_MAX],
    len: usize,
}

impl ReverseCache {
    fn from_first(data: &[u8]) -> Self {
        debug_assert!(!data.is_empty() && data.len() <= REVERSE_MAX);
        let len = data.len();
        let mut input = [0u8; REVERSE_MAX];
        let mut output = [0u8; REVERSE_MAX];
        input[..len].copy_from_slice(data);
        for i in 0..len {
            output[i] = data[len - 1 - i];
        }
        Self {
            input,
            output,
            len,
        }
    }

    fn matches(&self, data: &[u8]) -> bool {
        data.len() == self.len && data == &self.input[..self.len]
    }

    fn output(&self) -> &[u8] {
        &self.output[..self.len]
    }
}

struct Stats {
    quiet: bool,
    check_crc: bool,
    stats_interval: Duration,
    last_print: Mutex<Option<Instant>>,
    rx_pkts: AtomicU64,
    rx_bytes: AtomicU64,
    tx_pkts: AtomicU64,
    tx_bytes: AtomicU64,
    crc_ok: AtomicU64,
    crc_fail: AtomicU64,
    non_test: AtomicU64,
    lost: AtomicU64,
    dup: AtomicU64,
    seq: Mutex<SeqTracker>,
    icount: IcountWatch,
}

impl Stats {
    fn new(quiet: bool, check_crc: bool, stats_interval_ms: u64, icount: IcountWatch) -> Self {
        Self {
            quiet,
            check_crc,
            stats_interval: Duration::from_millis(stats_interval_ms),
            last_print: Mutex::new(None),
            rx_pkts: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            tx_pkts: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            crc_ok: AtomicU64::new(0),
            crc_fail: AtomicU64::new(0),
            non_test: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            dup: AtomicU64::new(0),
            seq: Mutex::new(SeqTracker::default()),
            icount,
        }
    }

    fn on_rx(&self, data: &[u8]) {
        self.rx_pkts.fetch_add(1, Ordering::Relaxed);
        self.rx_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        if self.check_crc {
            self.note_crc(data);
        }
        self.emit_traffic(data, "RX");
    }

    fn on_tx(&self, data: &[u8]) {
        self.tx_pkts.fetch_add(1, Ordering::Relaxed);
        self.tx_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.emit_traffic(data, "TX");
    }

    fn note_crc(&self, data: &[u8]) {
        for view in inspect(data) {
            match view {
                FrameView::Ok(frame) => {
                    self.crc_ok.fetch_add(1, Ordering::Relaxed);
                    let outcome = self.seq.lock().expect("序号锁损坏").observe(frame.seq);
                    self.lost.fetch_add(outcome.lost, Ordering::Relaxed);
                    self.dup.fetch_add(outcome.dup, Ordering::Relaxed);
                }
                FrameView::NotTest => {
                    self.non_test.fetch_add(1, Ordering::Relaxed);
                }
                FrameView::BadCrc => {
                    self.crc_fail.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    fn emit_traffic(&self, data: &[u8], dir: &str) {
        if self.quiet {
            if self.should_print() {
                self.print_counts("");
            }
        } else {
            log_bytes(dir, data);
        }
    }

    fn maybe_print(&self) {
        if self.quiet && self.should_print() {
            self.print_counts("");
        }
    }

    fn should_print(&self) -> bool {
        if self.stats_interval.is_zero() {
            return true;
        }
        let mut last = self.last_print.lock().expect("统计时间锁损坏");
        match *last {
            None => {
                *last = Some(Instant::now());
                true
            }
            Some(t) if t.elapsed() >= self.stats_interval => {
                *last = Some(Instant::now());
                true
            }
            Some(_) => false,
        }
    }

    fn print_counts(&self, prefix: &str) {
        let rx_pkts = self.rx_pkts.load(Ordering::Relaxed);
        let rx_bytes = self.rx_bytes.load(Ordering::Relaxed);
        let tx_pkts = self.tx_pkts.load(Ordering::Relaxed);
        let tx_bytes = self.tx_bytes.load(Ordering::Relaxed);
        let mut line = format!(
            "{prefix}RX {rx_pkts}包 {rx_bytes}字节  TX {tx_pkts}包 {tx_bytes}字节"
        );
        match self.icount.delta() {
            Some(c) => {
                line.push_str(&format!(
                    "  UART帧错{} 奇偶{} 溢出{}  UART错误率{}",
                    c.frame,
                    c.parity,
                    c.overrun,
                    pct(c.error_total(), rx_bytes)
                ));
            }
            None => line.push_str("  UART计数=不可用"),
        }
        if self.check_crc {
            let ok = self.crc_ok.load(Ordering::Relaxed);
            let fail = self.crc_fail.load(Ordering::Relaxed);
            let lost = self.lost.load(Ordering::Relaxed);
            let dup = self.dup.load(Ordering::Relaxed);
            let non_test = self.non_test.load(Ordering::Relaxed);
            line.push_str(&format!(
                "  CRC通过{ok} 失败{fail} 丢包{lost} 重复{dup} 非测试帧{non_test}  帧错误率{} 丢包率{}",
                pct(fail, ok.saturating_add(fail)),
                pct(lost, lost.saturating_add(ok))
            ));
        }
        log_line(&line);
    }

    fn info(&self, msg: &str) {
        if !self.quiet {
            log_line(msg);
        }
    }
}

fn pct(num: u64, den: u64) -> String {
    if den == 0 {
        "n/a".to_string()
    } else {
        format!("{:.4}%", (num as f64) * 100.0 / (den as f64))
    }
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
    let timeout = Duration::from_millis(cli.frame_idle_ms.max(5));

    let gpio = Gpio::open(cli.gpio)?;
    let port = open_port(&cli.device, cli.baud, format, timeout)?;
    let mut rt = Runtime {
        port,
        gpio,
        tx_setup: Duration::from_micros(cli.tx_setup_us),
        tx_hold: Duration::from_micros(cli.tx_hold_us),
    };

    let icount = IcountWatch::new(&rt.port);
    let stats = Arc::new(Stats::new(
        cli.quiet,
        cli.crc,
        cli.stats_interval_ms,
        icount,
    ));
    stats.info(&format!(
        "cfg device={} baud={} format={} gpio={} tx_setup={}us tx_hold={}us frame_idle={}ms crc={} stats_interval={}ms",
        cli.device,
        cli.baud,
        cli.format.to_ascii_uppercase(),
        cli.gpio,
        cli.tx_setup_us,
        cli.tx_hold_us,
        timeout.as_millis(),
        cli.crc,
        cli.stats_interval_ms
    ));

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
        Command::Duplex => run_duplex(rt, running, stats.clone())?,
        Command::Echo => run_echo(&mut rt, &running, &stats)?,
        Command::Send { text, file, stay } => {
            let payload = load_send_payload(text, file)?;
            run_send(&mut rt, &running, &payload, stay, &stats)?;
        }
        Command::Recv => run_recv(&mut rt, &running, &stats)?,
        Command::Traffic {
            count,
            payload,
            interval_ms,
        } => run_traffic(&mut rt, &running, &stats, count, payload, interval_ms)?,
        Command::Reverse => run_reverse(&mut rt, &running, &stats)?,
    }

    stats.print_counts("统计 ");
    Ok(())
}

fn load_send_payload(text: Option<String>, file: Option<PathBuf>) -> Result<Vec<u8>> {
    match (text, file) {
        (Some(_), Some(_)) => anyhow::bail!("send 不能同时指定文本和 --file"),
        (None, None) => anyhow::bail!("send 需要文本或 --file"),
        (Some(text), None) => Ok(text.into_bytes()),
        (None, Some(path)) => {
            std::fs::read(&path).with_context(|| format!("读取 {} 失败", path.display()))
        }
    }
}

fn run_duplex(rt: Runtime, running: Arc<AtomicBool>, stats: Arc<Stats>) -> Result<()> {
    stats.info("mode=duplex  stdin 每行发送，串口收包打印。Ctrl-C 退出。");
    let port = Arc::new(Mutex::new(rt.port));
    let gpio = Arc::new(rt.gpio);
    let tx_setup = rt.tx_setup;
    let tx_hold = rt.tx_hold;

    let rx_port = port.clone();
    let rx_running = running.clone();
    let rx_stats = stats.clone();
    let rx = thread::spawn(move || -> Result<()> {
        while rx_running.load(Ordering::SeqCst) {
            let mut guard = rx_port.lock().expect("串口锁损坏");
            match read_frame(&mut guard, MAX_IDLE_FRAME) {
                Ok(Some(frame)) => {
                    drop(guard);
                    rx_stats.on_rx(&frame);
                }
                Ok(None) => {
                    drop(guard);
                    rx_stats.maybe_print();
                }
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
        stats.on_tx(&data);
    }

    if io::stdin().is_terminal() {
        running.store(false, Ordering::SeqCst);
    } else {
        stats.info("stdin 已关闭，继续接收直到收到停止信号");
        while running.load(Ordering::SeqCst) {
            stats.maybe_print();
            thread::sleep(Duration::from_millis(50));
        }
    }
    running.store(false, Ordering::SeqCst);
    rx.join().expect("接收线程 panic")?;
    Ok(())
}

fn run_reverse(rt: &mut Runtime, running: &AtomicBool, stats: &Stats) -> Result<()> {
    rt.port
        .set_timeout(REVERSE_POLL)
        .context("设置 reverse 读超时失败")?;
    rt.gpio.set_rx()?;
    stats.info(
        "mode=reverse  首帧(≤8字节)倒序缓存，之后只比对；6s无接收则补发。Ctrl-C 退出。",
    );

    let mut cache: Option<ReverseCache> = None;
    let mut last_rx = Instant::now();
    let mut buf = [0u8; REVERSE_MAX];

    while running.load(Ordering::SeqCst) {
        match rt.port.read(&mut buf) {
            Ok(0) => {
                maybe_idle_reverse_tx(rt, cache.as_ref(), &mut last_rx, stats)?;
                stats.maybe_print();
            }
            Ok(n) => {
                let mut filled = n.min(REVERSE_MAX);
                if filled < REVERSE_MAX {
                    filled += drain_available(&mut rt.port, &mut buf[filled..])?;
                }
                let frame = &buf[..filled];
                stats.on_rx(frame);
                last_rx = Instant::now();

                let mut out_buf = [0u8; REVERSE_MAX];
                let reply_len = match &cache {
                    None => {
                        let c = ReverseCache::from_first(frame);
                        let n = c.len;
                        out_buf[..n].copy_from_slice(c.output());
                        cache = Some(c);
                        Some(n)
                    }
                    Some(c) if c.matches(frame) => {
                        let n = c.len;
                        out_buf[..n].copy_from_slice(c.output());
                        Some(n)
                    }
                    Some(_) => {
                        stats.info(&format!(
                            "reverse mismatch len={} hex={}",
                            frame.len(),
                            format_hex(frame)
                        ));
                        None
                    }
                };
                if let Some(n) = reply_len {
                    send_frame(
                        &mut rt.port,
                        &rt.gpio,
                        &out_buf[..n],
                        rt.tx_setup,
                        rt.tx_hold,
                    )?;
                    stats.on_tx(&out_buf[..n]);
                }
            }
            Err(err)
                if err.kind() == io::ErrorKind::TimedOut
                    || err.kind() == io::ErrorKind::WouldBlock =>
            {
                maybe_idle_reverse_tx(rt, cache.as_ref(), &mut last_rx, stats)?;
                stats.maybe_print();
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err).context("reverse 接收失败"),
        }
    }
    Ok(())
}

fn maybe_idle_reverse_tx(
    rt: &mut Runtime,
    cache: Option<&ReverseCache>,
    last_rx: &mut Instant,
    stats: &Stats,
) -> Result<()> {
    let Some(cache) = cache else {
        return Ok(());
    };
    if last_rx.elapsed() < REVERSE_IDLE_TX {
        return Ok(());
    }
    send_frame(
        &mut rt.port,
        &rt.gpio,
        cache.output(),
        rt.tx_setup,
        rt.tx_hold,
    )?;
    stats.on_tx(cache.output());
    *last_rx = Instant::now();
    Ok(())
}

fn run_echo(rt: &mut Runtime, running: &AtomicBool, stats: &Stats) -> Result<()> {
    stats.info("mode=echo  收到一帧后原样回发。Ctrl-C 退出。");
    while running.load(Ordering::SeqCst) {
        match read_frame(&mut rt.port, MAX_IDLE_FRAME) {
            Ok(Some(frame)) => {
                stats.on_rx(&frame);
                send_frame(
                    &mut rt.port,
                    &rt.gpio,
                    &frame,
                    rt.tx_setup,
                    rt.tx_hold,
                )?;
                stats.on_tx(&frame);
            }
            Ok(None) => stats.maybe_print(),
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
    stats: &Stats,
) -> Result<()> {
    anyhow::ensure!(!payload.is_empty(), "发送内容为空");
    send_frame(
        &mut rt.port,
        &rt.gpio,
        payload,
        rt.tx_setup,
        rt.tx_hold,
    )?;
    stats.on_tx(payload);
    if stay {
        stats.info("mode=send --stay  发完后继续接收。Ctrl-C 退出。");
        run_recv(rt, running, stats)?;
    }
    Ok(())
}

fn run_recv(rt: &mut Runtime, running: &AtomicBool, stats: &Stats) -> Result<()> {
    rt.gpio.set_rx()?;
    stats.info("mode=recv  只接收。Ctrl-C 退出。");
    while running.load(Ordering::SeqCst) {
        match read_frame(&mut rt.port, MAX_IDLE_FRAME) {
            Ok(Some(frame)) => stats.on_rx(&frame),
            Ok(None) => stats.maybe_print(),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err).context("recv 接收失败"),
        }
    }
    Ok(())
}

fn run_traffic(
    rt: &mut Runtime,
    running: &AtomicBool,
    stats: &Stats,
    count: u64,
    payload_len: usize,
    interval_ms: u64,
) -> Result<()> {
    anyhow::ensure!(payload_len > 0, "payload 必须大于 0");
    anyhow::ensure!(
        payload_len + crate::frame::OVERHEAD <= MAX_IDLE_FRAME,
        "payload 过大，最大 {} 字节",
        MAX_IDLE_FRAME - crate::frame::OVERHEAD
    );
    stats.info(&format!(
        "mode=traffic  count={count} payload={payload_len} interval={interval_ms}ms。Ctrl-C 退出。"
    ));
    let interval = Duration::from_millis(interval_ms);
    let mut seq = 0u32;
    let mut sent = 0u64;
    while running.load(Ordering::SeqCst) && (count == 0 || sent < count) {
        let payload = pattern_payload(seq, payload_len);
        let frame = encode(seq, &payload).map_err(|e| anyhow::anyhow!("{e}"))?;
        send_frame(
            &mut rt.port,
            &rt.gpio,
            &frame,
            rt.tx_setup,
            rt.tx_hold,
        )?;
        stats.on_tx(&frame);
        seq = seq.wrapping_add(1);
        sent += 1;
        if !interval.is_zero() && (count == 0 || sent < count) {
            sleep_while_running(interval, running, stats);
        } else {
            stats.maybe_print();
        }
    }
    Ok(())
}

fn sleep_while_running(total: Duration, running: &AtomicBool, stats: &Stats) {
    let start = Instant::now();
    while running.load(Ordering::SeqCst) && start.elapsed() < total {
        stats.maybe_print();
        let remain = total.saturating_sub(start.elapsed());
        thread::sleep(remain.min(Duration::from_millis(50)));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_cache_first_frame() {
        let c = ReverseCache::from_first(&[1, 2, 3, 4]);
        assert_eq!(c.output(), &[4, 3, 2, 1]);
        assert!(c.matches(&[1, 2, 3, 4]));
        assert!(!c.matches(&[1, 2, 3]));
        assert!(!c.matches(&[1, 2, 3, 5]));
        assert!(!c.matches(&[4, 3, 2, 1]));
    }

    #[test]
    fn reverse_cache_single_byte() {
        let c = ReverseCache::from_first(&[0xa5]);
        assert_eq!(c.output(), &[0xa5]);
        assert!(c.matches(&[0xa5]));
    }
}
