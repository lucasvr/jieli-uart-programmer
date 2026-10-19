//! DPS150 bench power supply, controlled with `dps150tool`.

use crate::find::{UsbSerial, siblings};
use eyre::{Result, WrapErr, ensure, eyre};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Clone)]
pub struct Psu {
    tool: PathBuf,
    port: String,
}

impl Psu {
    /// The DPS150's USB serial port.
    pub const USB: UsbSerial = UsbSerial {
        what: "DPS150",
        vid: 0x2e3c,
        pid: 0x5740,
        by_id: "/dev/serial/by-id/usb-Artery_AT32_Virtual_Com_Port_*",
    };
    const TOOL: &str = "dps150tool";
    const VOLTS: &str = "4.0";
    const AMPS: &str = "1.0";

    pub fn new(port: String) -> Result<Self> {
        Ok(Self {
            tool: Self::find_tool()?,
            port,
        })
    }

    /// Finds `dps150tool` in `PATH`, or built in a checkout next to this
    /// directory, like the SDK.
    fn find_tool() -> Result<PathBuf> {
        let in_path = std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(Self::TOOL).is_file()));
        if in_path {
            return Ok(PathBuf::from(Self::TOOL));
        }
        siblings(Self::TOOL)?
            .into_iter()
            .map(|d| d.join(Self::TOOL))
            .find(|c| c.is_file())
            .ok_or_else(|| eyre!("{} not found in PATH or next to this directory", Self::TOOL))
    }

    /// Switches the output on or off.
    pub fn set(&self, on: bool) -> Result<()> {
        let status = Command::new(&self.tool)
            .args(["-d", &self.port, "-u", Self::VOLTS, "-i", Self::AMPS])
            .args(["-x", "1", "-y", "1", "-z", "-o", if on { "1" } else { "0" }])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .wrap_err_with(|| format!("running {}", self.tool.display()))?;
        ensure!(
            status.success(),
            "{} failed ({status})",
            self.tool.display()
        );
        Ok(())
    }

    pub fn cycle(&self, off: Duration) -> Result<()> {
        self.cycle_with(off, || Ok(()))
    }

    /// Switches off, waits `off`, runs `before_on`, then switches on. "Off" is
    /// sent twice, matching the original bench scripts; "on" only once, since
    /// repeating it restarts the chip.
    pub fn cycle_with(&self, off: Duration, before_on: impl FnOnce() -> Result<()>) -> Result<()> {
        self.set(false)?;
        self.set(false)?;
        std::thread::sleep(off);
        before_on()?;
        self.set(true)
    }

    /// Cycles the supply in the background while the dongle waits for it.
    pub fn cycle_later(&self, off: Duration) -> std::thread::JoinHandle<Result<()>> {
        let psu = self.clone();
        std::thread::spawn(move || {
            // Give the dongle time to arm.
            std::thread::sleep(Duration::from_millis(500));
            psu.cycle(off)
        })
    }
}
