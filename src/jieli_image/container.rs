//! The SDK's "JLFV" containers, such as `sh55loader.uart` and `uboot.boot`.
//!
//! A 32-byte header and a table of 32-byte entries, each ENC-scrambled with
//! key 0xFFFF, then the payloads. All fields are little-endian.

use super::bytes::{u16_at, u32_at};
use super::cipher::Enc;
use super::crc::crc16;
use eyre::{Result, ensure, eyre};

const MAGIC: u32 = u32::from_le_bytes(*b"JLFV");

// Entry kinds.
const VERSION: u8 = 0;
const CODE: u8 = 1;
const LOAD_ADDRESS: u8 = 4;

#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: u8,
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Container {
    pub entries: Vec<Entry>,
}

impl Container {
    pub fn parse(file: &[u8]) -> Result<Self> {
        ensure!(file.len() >= 32, "container too short");
        let hdr = Enc::copy(0xffff, &file[..32]);
        let magic = u32_at(&hdr, 0);
        ensure!(magic == 0 || magic == MAGIC, "bad magic {magic:#x}");
        ensure!(u16_at(&hdr, 4) == crc16(&hdr[6..32]), "header CRC mismatch");
        let size = u32_at(&hdr, 8) as usize;
        ensure!(
            size == file.len(),
            "size {size} != file length {}",
            file.len()
        );
        let count = u32_at(&hdr, 12) as usize;
        let table_end = 32 + count * 32;
        ensure!(table_end <= file.len(), "entry table truncated");
        ensure!(
            u16_at(&hdr, 6) == crc16(&file[32..table_end]),
            "entry table CRC mismatch"
        );

        let mut entries = Vec::with_capacity(count);
        for raw in file[32..table_end].chunks(32) {
            let e = Enc::copy(0xffff, raw);
            let off = u32_at(&e, 4) as usize;
            let len = u32_at(&e, 8) as usize;
            let name_end = e[16..].iter().position(|&c| c == 0).map_or(32, |p| 16 + p);
            let name = String::from_utf8_lossy(&e[16..name_end]).into_owned();
            ensure!(off + len <= file.len(), "entry {name:?} out of range");
            let data = file[off..off + len].to_vec();
            ensure!(crc16(&data) == u16_at(&e, 2), "entry {name:?} CRC mismatch");
            entries.push(Entry {
                kind: e[0],
                name,
                data,
            });
        }
        Ok(Self { entries })
    }

    fn kind(&self, kind: u8) -> Option<&Entry> {
        self.entries.iter().find(|e| e.kind == kind)
    }

    /// Returns the code entry: the loader in a `.uart`, the bank image in a
    /// `.boot`.
    pub fn code(&self) -> Result<&[u8]> {
        Ok(&self.kind(CODE).ok_or_else(|| eyre!("no code entry"))?.data)
    }

    /// Returns the load address, if present.
    pub fn load_address(&self) -> Option<u32> {
        self.kind(LOAD_ADDRESS)
            .filter(|e| e.data.len() == 4)
            .map(|e| u32_at(&e.data, 0))
    }

    /// Returns the version string, e.g. "LOADER-v1.0.0-...".
    pub fn version(&self) -> Option<String> {
        let data = &self.kind(VERSION)?.data;
        let s = data.get(4..)?;
        let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
        Some(String::from_utf8_lossy(&s[..end]).into_owned())
    }
}
