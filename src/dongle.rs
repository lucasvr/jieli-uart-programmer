//! Client for the jieli-programmer dongle: an ESP32-C6 that sends the boot
//! ROM key and bridges the chip's single-wire UART on PB9.
//!
//! Frame format, in both directions: `A5 | type | len u16 LE | payload | crc8`.

use crate::boot::BootParams;
use crate::find::UsbSerial;
use crate::jieli_image::crc8;
use eyre::{Result, WrapErr, bail, ensure};
use serialport::SerialPort;
use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

const SYNC: u8 = 0xa5;
const MAX_PAYLOAD: usize = 12 * 1024;

/// Driver modes for the UART line.
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum Driver {
    /// Open-drain UART pad.
    Uart = 0,
}

/// Outcome of the ROM stage.
pub struct BootResult {
    /// 0 = ok, 1 = no power-off seen, 2 = no power-on seen.
    pub status: u8,
    /// 0 = no ROM ack, 1 = ROM ack but no loader reply, 2 = loader replied.
    pub stage: u8,
    /// Baud rate of the loader's reply.
    pub loader_baud: u32,
}

pub struct Dongle {
    port: Box<dyn SerialPort>,
    line_baud: u32,
    verbose: bool,
}

impl Dongle {
    /// The ESP32-C6's USB serial/JTAG port.
    pub const USB: UsbSerial = UsbSerial {
        what: "dongle",
        vid: 0x303a,
        pid: 0x1001,
        by_id: "/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_*",
    };

    /// Opens the dongle and checks that it responds. A dongle interrupted
    /// mid-command can stop reading USB, so on failure it is reset and
    /// tried once more.
    pub fn open(path: &str, verbose: bool) -> Result<Self> {
        match Self::connect(path, verbose) {
            Ok(d) => Ok(d),
            Err(e) => {
                eprintln!("dongle: no answer ({e}), resetting it");
                Self::reset(path)?;
                Self::connect(path, verbose).wrap_err("the dongle did not answer after a reset")
            }
        }
    }

    /// Resets the ESP32-C6 like esptool does. With RTS set and DTR clear, the
    /// USB serial/JTAG peripheral holds the chip in reset, which works even
    /// when the firmware is hung.
    fn reset(path: &str) -> Result<()> {
        let mut port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(100))
            .open()
            .wrap_err_with(|| format!("opening dongle {path}"))?;
        port.write_data_terminal_ready(false)?;
        port.write_request_to_send(true)?;
        std::thread::sleep(Duration::from_millis(200));
        port.write_request_to_send(false)?;
        drop(port);
        // Wait for the chip to re-enumerate on USB.
        std::thread::sleep(Duration::from_millis(500));
        let deadline = Instant::now() + Duration::from_secs(10);
        while serialport::new(path, 115_200).open().is_err() {
            ensure!(
                Instant::now() < deadline,
                "the dongle did not come back at {path} after a reset"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        std::thread::sleep(Duration::from_millis(500));
        Ok(())
    }

    fn connect(path: &str, verbose: bool) -> Result<Self> {
        let port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(100))
            .open()
            .wrap_err_with(|| format!("opening dongle {path}"))?;
        let mut d = Self {
            port,
            line_baud: 9600,
            verbose,
        };
        // Discard stale data from an interrupted run.
        std::thread::sleep(Duration::from_millis(100));
        d.port.clear(serialport::ClearBuffer::All)?;
        let mut junk = [0u8; 256];
        while matches!(d.port.read(&mut junk), Ok(n) if n > 0) {}
        d.send_frame(b'V', &[])?;
        let deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (ty, payload) = d.recv_frame(left.max(Duration::from_millis(1)))?;
            if ty == b'V' {
                eprintln!("dongle: {}", String::from_utf8_lossy(&payload));
                return Ok(d);
            }
        }
    }

    fn send_frame(&mut self, ty: u8, payload: &[u8]) -> Result<()> {
        ensure!(
            payload.len() <= MAX_PAYLOAD,
            "{} bytes is too large a frame",
            payload.len()
        );
        let len = (payload.len() as u16).to_le_bytes();
        let mut f = vec![SYNC, ty, len[0], len[1]];
        f.extend_from_slice(payload);
        f.push(crc8(&f[1..]));
        self.port.write_all(&f)?;
        self.port.flush()?;
        Ok(())
    }

    fn read_byte(&mut self, deadline: Instant) -> Result<u8> {
        let mut b = [0u8; 1];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            ensure!(!left.is_zero(), "timeout waiting for the dongle");
            self.port
                .set_timeout(left.min(Duration::from_millis(100)))?;
            match self.port.read(&mut b) {
                Ok(1) => return Ok(b[0]),
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::TimedOut => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn recv_frame(&mut self, timeout: Duration) -> Result<(u8, Vec<u8>)> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.read_byte(deadline)? != SYNC {
                continue;
            }
            let mut f = vec![self.read_byte(deadline)?];
            f.push(self.read_byte(deadline)?);
            f.push(self.read_byte(deadline)?);
            let len = usize::from(u16::from_le_bytes([f[1], f[2]]));
            for _ in 0..len {
                f.push(self.read_byte(deadline)?);
            }
            if crc8(&f) != self.read_byte(deadline)? {
                eprintln!("dongle: bad frame CRC, resynchronising");
                continue;
            }
            let payload = f.split_off(3);
            if f[0] == b'E' {
                bail!("dongle error: {}", String::from_utf8_lossy(&payload));
            }
            return Ok((f[0], payload));
        }
    }

    fn call(&mut self, ty: u8, payload: &[u8], timeout: Duration) -> Result<(u8, Vec<u8>)> {
        self.send_frame(ty, payload)?;
        self.recv_frame(timeout)
    }

    /// Switches the dongle's power pin; `on` also releases the line.
    pub fn power(&mut self, on: bool) -> Result<()> {
        self.call(b'P', &[on.into()], Duration::from_millis(500))?;
        Ok(())
    }

    pub fn set_driver(&mut self, driver: Driver) -> Result<()> {
        self.call(b'S', &[driver as u8], Duration::from_millis(500))?;
        Ok(())
    }

    /// Runs the ROM stage: waits for power, sends the key and header, and
    /// pushes the loader. This is a single command because the ROM's window
    /// is too short for a host round trip.
    pub fn boot(&mut self, p: &BootParams, header: &[u8], image: &[u8]) -> Result<BootResult> {
        let payload = p.payload(header, image);
        let (ty, r) = self.call(b'K', &payload, p.timeout(image.len()))?;
        ensure!(
            ty == b'K' && r.len() >= 16,
            "unexpected reply {ty:#04x} to the key command"
        );
        let result = BootResult {
            status: r[0],
            stage: r[11],
            loader_baud: u32::from_le_bytes(r[12..16].try_into().unwrap()),
        };
        self.line_baud = if result.stage == 2 {
            result.loader_baud
        } else {
            p.rom_baud
        };
        Ok(result)
    }

    pub fn set_baud(&mut self, baud: u32) -> Result<()> {
        if baud == self.line_baud {
            return Ok(());
        }
        let (_, r) = self.call(b'B', &baud.to_le_bytes(), Duration::from_millis(500))?;
        ensure!(r.first() == Some(&0), "dongle rejected {baud} baud");
        self.line_baud = baud;
        Ok(())
    }

    /// Discards data received on the line.
    pub fn flush(&mut self) -> Result<()> {
        self.call(b'F', &[], Duration::from_millis(500))?;
        self.port.clear(serialport::ClearBuffer::Input)?;
        Ok(())
    }

    /// Writes to the line and waits until the bytes are sent.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        if self.verbose {
            eprintln!("[tx {:4}] {}", data.len(), hex(data));
        }
        for chunk in data.chunks(MAX_PAYLOAD) {
            let ms = (chunk.len() as u64 * 10_000) / u64::from(self.line_baud.max(1)) + 1000;
            let (_, r) = self.call(b'D', chunk, Duration::from_millis(ms))?;
            ensure!(r.first() == Some(&0), "dongle failed to write to the line");
        }
        Ok(())
    }

    /// Reads up to `max` bytes, waiting at most `timeout` for the first.
    pub fn read(&mut self, max: usize, timeout: Duration) -> Result<Vec<u8>> {
        let mut p = (max.min(MAX_PAYLOAD) as u16).to_le_bytes().to_vec();
        p.extend_from_slice(&(timeout.as_millis().min(60_000) as u16).to_le_bytes());
        let (ty, r) = self.call(b'R', &p, timeout + Duration::from_secs(1))?;
        ensure!(ty == b'D', "unexpected reply {ty:#04x} to a read");
        if self.verbose && !r.is_empty() {
            eprintln!("[rx {:4}] {}", r.len(), hex(&r));
        }
        Ok(r)
    }

    /// Reads into `buf` until it is full or `timeout` expires. Returns the
    /// number of bytes read.
    pub fn read_exact(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        let deadline = Instant::now() + timeout;
        let mut got = 0;
        while got < buf.len() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let chunk = self.read(buf.len() - got, left.min(Duration::from_millis(500)))?;
            buf[got..got + chunk.len()].copy_from_slice(&chunk);
            got += chunk.len();
        }
        Ok(got)
    }

    pub fn verbose(&self) -> bool {
        self.verbose
    }
}

fn hex(data: &[u8]) -> String {
    let mut s: String = data.iter().take(48).map(|b| format!("{b:02x} ")).collect();
    if data.len() > 48 {
        s.push_str("...");
    }
    s
}
