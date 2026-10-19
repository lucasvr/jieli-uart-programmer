//! Boot ROM stage: send the key on PB9 after power-on, then the header and
//! the SDK's RAM loader.

use crate::jieli_image::{Container, KeyBlob, crc8, crc16};
use eyre::{Result, WrapErr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The RAM loader from JieLi's AD15N SDK; see `vendor/fw-AD15N`.
pub const LOADER: &[u8] = include_bytes!("../vendor/fw-AD15N/sh55loader.uart");
/// The second-stage bootloader from JieLi's AD15N SDK.
pub const UBOOT: &[u8] = include_bytes!("../vendor/fw-AD15N/uboot.boot");

/// RAM loader that the dongle sends to the boot ROM.
pub struct Loader {
    /// Loader code followed by the key blob.
    pub image: Vec<u8>,
    /// Load address.
    pub addr: u32,
    pub version: String,
}

impl Loader {
    /// SH55 loaders carry no address; they run from 0.
    const DEFAULT_ADDR: u32 = 0;

    pub fn parse(data: &[u8]) -> Result<Self> {
        let c = Container::parse(data).wrap_err("parsing the loader")?;
        let seed = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64;
        let mut image = c.code()?.to_vec();
        // Key blob for 0xFFFF, i.e. no chip key.
        image.extend_from_slice(KeyBlob::random(0xffff, seed).bytes());
        Ok(Self {
            image,
            addr: c.load_address().unwrap_or(Self::DEFAULT_ADDR),
            version: c.version().unwrap_or_default(),
        })
    }

    /// Builds the 21-byte header the ROM expects before the image.
    pub fn header(&self, p: &BootParams) -> [u8; 21] {
        let mut h = [0u8; 21];
        h[1..5].copy_from_slice(&[0x55, 0xaa, 0x10, 0x20]);
        h[5..9].copy_from_slice(&self.addr.to_le_bytes());
        h[9..13].copy_from_slice(&(self.image.len() as u32).to_le_bytes());
        h[13..15].copy_from_slice(&crc16(&self.image).to_le_bytes());
        let crc = crc16(&h[5..15]);
        h[15..17].copy_from_slice(&crc.to_le_bytes());
        h[17] = 10; // SERIAL_CMD_OPT
        h[18] = (p.loader_baud / 10_000) as u8; // SERIAL_CMD_RATE, in 10 kbaud
        h[19] = 0;
        h[20] = crc8(&h[1..20]);
        h
    }
}

/// ROM stage timing and baud rates. The defaults are tested on the AD152A4.
pub struct BootParams {
    /// Delay from power-on to the key, in us.
    pub delay_us: u32,
    /// Key bit period, in us.
    pub key_bit_us: u16,
    /// ROM ack timeout, in ms.
    pub ack_ms: u16,
    /// Delay from the ROM ack to the loader push, in ms.
    pub push_delay_ms: u16,
    /// ROM baud rate.
    pub rom_baud: u32,
    /// Baud rate of the loader push and the loader link.
    pub loader_baud: u32,
}

impl Default for BootParams {
    fn default() -> Self {
        Self {
            delay_us: 17_000,
            key_bit_us: 1000,
            ack_ms: 300,
            push_delay_ms: 150,
            rom_baud: 9600,
            loader_baud: 100_000,
        }
    }
}

impl BootParams {
    /// Tells the dongle to wait for an external power cycle.
    const MANUAL_POWER: u16 = 0xffff;
    /// Push the loader as soon as the ROM acks.
    const FLAG_PUSH: u8 = 2;

    /// Builds the payload of the dongle's `K` command.
    pub fn payload(&self, header: &[u8], image: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(32 + header.len() + image.len());
        p.extend_from_slice(&Self::MANUAL_POWER.to_le_bytes());
        p.extend_from_slice(&self.delay_us.to_le_bytes());
        p.extend_from_slice(&self.key_bit_us.to_le_bytes());
        p.push(1); // key repeat
        p.push(0); // polarity: high pulses on a low idle
        p.extend_from_slice(&self.ack_ms.to_le_bytes());
        p.extend_from_slice(&((1_000_000 / self.rom_baud) as u16).to_le_bytes());
        p.push(Self::FLAG_PUSH);
        p.extend_from_slice(&self.loader_baud.to_le_bytes());
        p.extend_from_slice(&self.loader_baud.to_le_bytes()); // expected reply rate
        p.push(header.len() as u8);
        p.extend_from_slice(&self.rom_baud.to_le_bytes());
        p.extend_from_slice(&self.push_delay_ms.to_le_bytes());
        p.push(0); // no first command
        p.extend_from_slice(header);
        p.extend_from_slice(image);
        p
    }

    /// Upper bound for the `K` command: 130 s for the power cycle, plus the
    /// key, ack and push.
    pub fn timeout(&self, image_len: usize) -> Duration {
        let push_ms = image_len as u64 * 10_000 / u64::from(self.loader_baud);
        Duration::from_millis(
            130_000
                + u64::from(self.delay_us) / 1000
                + u64::from(self.key_bit_us) * 18 / 1000
                + u64::from(self.ack_ms)
                + u64::from(self.push_delay_ms)
                + push_ms
                + 2000,
        )
    }
}
