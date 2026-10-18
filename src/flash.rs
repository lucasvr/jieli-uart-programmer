//! Flash operations over the loader link.

use crate::dongle::Dongle;
use crate::jieli_image::{crc16, flash_key};
use crate::link::Link;
use eyre::{Result, ensure};
use std::io::Write as _;

pub const SECTOR: u32 = 0x1000;

pub struct DeviceInfo {
    pub flash_id: u32,
    pub flash_size: Option<u32>,
    pub buffer_size: u32,
    pub erase_unit: u32,
    pub chip_keys: [u16; 3],
}

impl DeviceInfo {
    /// Key that scrambles the application area.
    pub fn flash_key(&self) -> u16 {
        flash_key(self.chip_keys[0])
    }
}

impl std::fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        writeln!(f, "flash id    : {:#010x}", self.flash_id)?;
        match self.flash_size {
            Some(s) => writeln!(f, "flash size  : {s:#x} ({} KiB)", s / 1024)?,
            None => writeln!(f, "flash size  : unknown")?,
        }
        writeln!(f, "buffer size : {:#x}", self.buffer_size)?;
        writeln!(f, "erase unit  : {:#x}", self.erase_unit)?;
        let [a, b, c] = self.chip_keys;
        writeln!(f, "chip keys   : {a:#06x} {b:#06x} {c:#06x}")?;
        write!(f, "flash key   : {:#06x}", self.flash_key())
    }
}

/// Progress indicator printed to stderr.
pub struct Progress(&'static str);

impl Progress {
    pub fn new(label: &'static str) -> Self {
        Self(label)
    }

    fn update(&self, done: u32, total: u32) {
        let pct = u64::from(done) * 100 / u64::from(total.max(1));
        eprint!("\r{} {pct:3}% ({done:#x} of {total:#x})", self.0);
        if done >= total {
            eprintln!();
        }
        let _ = std::io::stderr().flush();
    }
}

pub struct Flash {
    link: Link,
    pub info: DeviceInfo,
}

impl Flash {
    /// Swaps the link key and queries the chip, in the same order as the
    /// vendor tool.
    pub fn open(mut link: Link) -> Result<Self> {
        link.swap_key()?;
        let chip_keys = link.read_chip_key()?;
        let flash_id = link.read_device_id()?;
        let buffer_size = match link.query_buffer_size()? {
            Some(n) if (0x100..=0x10000).contains(&n) => n,
            _ => SECTOR,
        };
        let erase_unit = link.read_erase_unit().unwrap_or(SECTOR);
        // The low byte of the JEDEC id is log2 of the size.
        let density = (flash_id & 0xff).checked_sub(0x10).filter(|d| *d <= 16);
        let info = DeviceInfo {
            flash_id,
            flash_size: density.map(|d| 0x1_0000 << d),
            buffer_size,
            erase_unit,
            chip_keys,
        };
        Ok(Self { link, info })
    }

    pub fn into_dongle(self) -> Dongle {
        self.link.into_dongle()
    }

    fn chunk(&self) -> u32 {
        self.info.buffer_size.clamp(256, SECTOR)
    }

    pub fn read(&mut self, addr: u32, len: u32, progress: &Progress) -> Result<Vec<u8>> {
        let step = self.chunk();
        let mut out = Vec::with_capacity(len as usize);
        while (out.len() as u32) < len {
            let off = out.len() as u32;
            let n = step.min(len - off);
            let chunk = self.link.read_buffer(addr + off, n)?;
            ensure!(chunk.len() == n as usize, "short read at {:#x}", addr + off);
            out.extend_from_slice(&chunk);
            progress.update(off + n, len);
        }
        Ok(out)
    }

    /// Writes sector by sector, skipping sectors whose CRC already matches,
    /// and verifies each one.
    pub fn write(&mut self, addr: u32, data: &[u8], progress: &Progress) -> Result<()> {
        ensure!(
            addr.is_multiple_of(SECTOR),
            "{addr:#x} is not sector aligned"
        );
        let step = self.chunk() as usize;
        let total = data.len() as u32;
        for (i, sector) in data.chunks(SECTOR as usize).enumerate() {
            let a = addr + i as u32 * SECTOR;
            let want = crc16(sector);
            let len = sector.len() as u32;
            if self.link.flash_crc16(a, len)? != want {
                self.link.erase_sector(a)?;
                for (j, part) in sector.chunks(step).enumerate() {
                    self.link.write_buffer(a + (j * step) as u32, part)?;
                }
                let got = self.link.flash_crc16(a, len)?;
                ensure!(
                    got == want,
                    "verify failed at {a:#x}: {got:#06x} != {want:#06x}"
                );
            }
            progress.update(a - addr + len, total);
        }
        self.link.data_done()
    }
}
