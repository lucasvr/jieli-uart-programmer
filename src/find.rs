//! Finds the hardware and the tools around it.

use eyre::{Result, WrapErr, eyre};
use serialport::SerialPortType;
use std::path::PathBuf;

/// A USB serial device, identified by its vendor and product ids.
pub struct UsbSerial {
    pub what: &'static str,
    pub vid: u16,
    pub pid: u16,
    /// Linux `/dev/serial/by-id` pattern, tried first.
    pub by_id: &'static str,
}

impl UsbSerial {
    /// Returns `explicit`, or the first serial port of this device.
    pub fn find(&self, explicit: Option<String>) -> Result<String> {
        if let Some(p) = explicit {
            return Ok(p);
        }
        if let Some(p) = glob::glob(self.by_id)?.flatten().next() {
            return Ok(p.display().to_string());
        }
        let mut ports: Vec<String> = serialport::available_ports()
            .wrap_err("listing the serial ports")?
            .into_iter()
            .filter(|p| match &p.port_type {
                SerialPortType::UsbPort(u) => u.vid == self.vid && u.pid == self.pid,
                _ => false,
            })
            .map(|p| p.port_name)
            // macOS lists each port as /dev/tty.* and /dev/cu.*; only the
            // latter opens without waiting for carrier detect.
            .filter(|p| !cfg!(target_os = "macos") || p.starts_with("/dev/cu."))
            .collect();
        ports.sort();
        ports.into_iter().next().ok_or_else(|| {
            eyre!(
                "no {} found (USB {:04x}:{:04x}); is it plugged in?",
                self.what,
                self.vid,
                self.pid
            )
        })
    }
}

/// `name` next to the current directory, the executable, or any of their
/// ancestors, nearest first.
pub fn siblings(name: &str) -> Result<Vec<PathBuf>> {
    let cwd = std::env::current_dir()?;
    let exe = std::env::current_exe()?;
    Ok(cwd
        .ancestors()
        .chain(exe.ancestors())
        .map(|a| a.join(name))
        .collect())
}
