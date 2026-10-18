//! DPS150 bench power supply, controlled with `dps150tool`.

use eyre::{Result, WrapErr, ensure};
use std::process::{Command, Stdio};
use std::time::Duration;

pub struct Psu {
    port: String,
}

impl Psu {
    /// Linux by-id path pattern.
    pub const BY_ID: &str = "/dev/serial/by-id/usb-Artery_AT32_Virtual_Com_Port_*";
    const VOLTS: &str = "4.0";
    const AMPS: &str = "1.0";

    pub fn new(port: String) -> Self {
        Self { port }
    }

    /// Switches the output on or off.
    pub fn set(&self, on: bool) -> Result<()> {
        let status = Command::new("dps150tool")
            .args(["-d", &self.port, "-u", Self::VOLTS, "-i", Self::AMPS])
            .args(["-x", "1", "-y", "1", "-z", "-o", if on { "1" } else { "0" }])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .wrap_err("running dps150tool")?;
        ensure!(status.success(), "dps150tool failed ({status})");
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
        let psu = Self::new(self.port.clone());
        std::thread::spawn(move || {
            // Give the dongle time to arm.
            std::thread::sleep(Duration::from_millis(500));
            psu.cycle(off)
        })
    }
}
