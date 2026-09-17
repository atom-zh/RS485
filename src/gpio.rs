use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

/// QCM6125 手册：GPIO 拉高 = RX，拉低 = TX。
pub const LEVEL_RX: &str = "1";
pub const LEVEL_TX: &str = "0";

#[derive(Clone, Debug)]
pub struct Gpio {
    pin: u32,
    value_path: PathBuf,
}

impl Gpio {
    pub fn open(pin: u32) -> Result<Self> {
        let gpio_dir = PathBuf::from(format!("/sys/class/gpio/gpio{pin}"));
        if !gpio_dir.exists() {
            export_pin(pin)?;
            wait_for_path(&gpio_dir.join("direction"), Duration::from_secs(1))?;
        }

        fs::write(gpio_dir.join("direction"), "out")
            .with_context(|| format!("设置 GPIO{pin} direction=out 失败"))?;

        let gpio = Self {
            pin,
            value_path: gpio_dir.join("value"),
        };
        gpio.set_rx()?;
        Ok(gpio)
    }

    pub fn set_tx(&self) -> Result<()> {
        self.write_level(LEVEL_TX)
            .with_context(|| format!("GPIO{} 拉低(TX) 失败", self.pin))
    }

    pub fn set_rx(&self) -> Result<()> {
        self.write_level(LEVEL_RX)
            .with_context(|| format!("GPIO{} 拉高(RX) 失败", self.pin))
    }

    fn write_level(&self, level: &str) -> io::Result<()> {
        fs::write(&self.value_path, level)
    }
}

impl Drop for Gpio {
    fn drop(&mut self) {
        let _ = self.set_rx();
    }
}

fn export_pin(pin: u32) -> Result<()> {
    match fs::write("/sys/class/gpio/export", pin.to_string()) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::EBUSY) => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "export GPIO{pin} 失败（需要 root，路径 /sys/class/gpio/export）"
            )
        }),
    }
}

fn wait_for_path(path: &Path, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    anyhow::bail!("等待 {} 超时", path.display());
}
