//! The flash image the SH54/SH55 boot ROM starts from.
//!
//! ```text
//! 0x0000  flash header, ENC(0xFFFF)
//! 0x0020  top-level entries, ENC(0xFFFF): uboot.boot, isd_config.ini, app_dir_head
//! 0x0080  uboot.boot (bank header and data scrambled apart), then the config
//! 0x1000  application area, scrambled with the chip's flash key:
//!         app_area_head, then app.bin, VM and EEPROM entries, then app.bin at +0x100
//! ```
//!
//! The application area is mapped at 0x100000, so the app starts at
//! 0x100100. The image ends where VM begins, so writing it preserves the
//! device's saved settings.

use super::bytes::{u16_at, u32_at};
use super::cipher::{Enc, KeyBlob};
use super::container::Container;
use super::crc::crc16;
use eyre::{Result, ensure, eyre};

/// Start of the application area.
const AREA: usize = 0x1000;
/// Where the app runs from.
const ENTRY_POINT: u32 = 0x10_0100;
/// Offset of app.bin in the area.
const APP_OFF: usize = 0x100;
const ENTRY_LEN: usize = 32;
const APP_ALIGN: usize = 0x1000;
const VM_ALIGN: usize = 0x100;
const EEPROM_SIZE: u32 = 0x2000;

/// Raw name fields from a stock image; the bytes after the NUL are data.
const VM_NAME: [u8; 16] = *b"VM\0\xff\xff\xff\xff\xff\xff\xff\xff\xff\x00\x04\x00\x00";
const EEPROM_NAME: [u8; 16] = *b"EEPROM\0\xff\xff\xff\xff\xffAUTO";

fn align(n: usize, a: usize) -> usize {
    n.div_ceil(a) * a
}

/// A 32-byte JLFS directory entry.
#[derive(Debug, Clone)]
struct DirEntry {
    data_crc: u16,
    off: u32,
    size: u32,
    attr: u8,
    res: u8,
    /// Non-zero on the last entry of a list.
    last: u16,
    name: [u8; 16],
}

impl DirEntry {
    fn new(name: &str, attr: u8, res: u8, off: u32, size: u32, data_crc: u16) -> Self {
        let mut raw = [0xff; 16];
        let n = &name.as_bytes()[..name.len().min(15)];
        raw[..n.len()].copy_from_slice(n);
        raw[n.len()] = 0;
        Self {
            data_crc,
            off,
            size,
            attr,
            res,
            last: 0,
            name: raw,
        }
    }

    fn with_name(mut self, raw: [u8; 16]) -> Self {
        self.name = raw;
        self
    }

    fn last(mut self) -> Self {
        self.last = 1;
        self
    }

    fn to_bytes(&self) -> [u8; ENTRY_LEN] {
        let mut b = [0u8; ENTRY_LEN];
        b[2..4].copy_from_slice(&self.data_crc.to_le_bytes());
        b[4..8].copy_from_slice(&self.off.to_le_bytes());
        b[8..12].copy_from_slice(&self.size.to_le_bytes());
        b[12] = self.attr;
        b[13] = self.res;
        b[14..16].copy_from_slice(&self.last.to_le_bytes());
        b[16..].copy_from_slice(&self.name);
        let crc = crc16(&b[2..]);
        b[..2].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// `None` when the entry's CRC does not match.
    fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < ENTRY_LEN || crc16(&b[2..ENTRY_LEN]) != u16_at(b, 0) {
            return None;
        }
        Some(Self {
            data_crc: u16_at(b, 2),
            off: u32_at(b, 4),
            size: u32_at(b, 8),
            attr: b[12],
            res: b[13],
            last: u16_at(b, 14),
            name: b[16..32].try_into().unwrap(),
        })
    }

    fn name(&self) -> String {
        let end = self.name.iter().position(|&c| c == 0).unwrap_or(16);
        String::from_utf8_lossy(&self.name[..end]).into_owned()
    }
}

/// The SFC keystream of the application area: 32-byte blocks, each keyed
/// with `key ^ (offset in the area >> 2)`.
struct AreaCipher(u16);

impl AreaCipher {
    /// Scrambles `data`, found at flash address `addr` (>= 0x1000).
    fn apply(&self, data: &mut [u8], addr: usize) {
        let mut i = 0;
        while i < data.len() {
            let rel = addr + i - AREA;
            let block = rel & !31;
            let mut ks = [0u8; 32];
            Enc::new(self.0 ^ (block >> 2) as u16).apply(&mut ks);
            let j = rel - block;
            let n = (32 - j).min(data.len() - i);
            for (d, k) in data[i..i + n].iter_mut().zip(&ks[j..j + n]) {
                *d ^= k;
            }
            i += n;
        }
    }

    fn copy(&self, data: &[u8], addr: usize) -> Vec<u8> {
        let mut out = data.to_vec();
        self.apply(&mut out, addr);
        out
    }
}

/// The second-stage bootloader (a "BANKCB" image) from the SDK's `uboot.boot`.
pub struct Uboot(Vec<u8>);

impl Uboot {
    /// Parses an SDK `uboot.boot` container or a bare bank image.
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let bank = match Container::parse(raw) {
            Ok(c) => c.code()?.to_vec(),
            Err(_) => raw.to_vec(),
        };
        ensure!(bank.len() >= 16, "uboot image too short");
        let size = usize::from(u16_at(&bank, 2));
        let off = u32_at(&bank, 8) as usize;
        ensure!(
            crc16(&bank[..14]) == u16_at(&bank, 14)
                && off + size <= bank.len()
                && crc16(&bank[off..off + size]) == u16_at(&bank, 12),
            "not a valid uboot bank image"
        );
        Ok(Self(bank[..off + size].to_vec()))
    }

    /// Offset of the bank data, after its header.
    fn data_offset(&self) -> usize {
        u32_at(&self.0, 8) as usize
    }
}

/// Builds flash images for one chip.
pub struct Packer {
    /// The chip's flash key, as printed by `jieli-uart info`.
    pub key: u16,
    pub flash_size: u32,
    pub uboot: Uboot,
}

impl Packer {
    /// Packs a flat app binary linked at 0x100100.
    pub fn pack(&self, app: &[u8]) -> Result<FlashImage> {
        let mut app = app.to_vec();
        app.resize(align(app.len(), APP_ALIGN), 0xff);
        let area_size = APP_OFF + app.len();
        let vm_start = align(AREA + area_size, VM_ALIGN);
        let eeprom = self.flash_size - EEPROM_SIZE;
        ensure!(
            vm_start as u32 <= eeprom,
            "app does not fit: it ends at {vm_start:#x}, EEPROM starts at {eeprom:#x}"
        );

        let mut body = Vec::with_capacity(area_size);
        let entries = [
            DirEntry::new(
                "app.bin",
                0x82,
                0xff,
                APP_OFF as u32,
                app.len() as u32,
                crc16(&app),
            ),
            DirEntry::new(
                "VM",
                0x12,
                0x81,
                vm_start as u32,
                eeprom - vm_start as u32,
                0xffff,
            )
            .with_name(VM_NAME),
            DirEntry::new("EEPROM", 0x92, 0x81, eeprom, EEPROM_SIZE, 0xffff)
                .with_name(EEPROM_NAME)
                .last(),
        ];
        for e in &entries {
            body.extend_from_slice(&e.to_bytes());
        }
        body.resize(APP_OFF - ENTRY_LEN, 0xff);
        body.extend_from_slice(&app);
        let head = DirEntry::new(
            "app_area_head",
            0x83,
            0x02,
            ENTRY_POINT,
            area_size as u32,
            crc16(&body),
        );

        let mut out = vec![0xff; vm_start];
        out[..AREA].copy_from_slice(&self.boot_region()?);
        let mut area = head.to_bytes().to_vec();
        area.extend_from_slice(&body);
        AreaCipher(self.key).apply(&mut area, AREA);
        out[AREA..AREA + area_size].copy_from_slice(&area);
        Ok(FlashImage(out))
    }

    /// Flash 0 to 0x1000: header, top-level entries, uboot and config.
    fn boot_region(&self) -> Result<Vec<u8>> {
        let bank = &self.uboot.0;
        let cfg = Self::config_blob();
        let uboot_off = 0x20 + 3 * ENTRY_LEN;
        let cfg_off = uboot_off + bank.len();
        ensure!(
            cfg_off + cfg.len() <= AREA,
            "uboot and config do not fit below 0x1000"
        );

        let mut out = vec![0xff; AREA];
        out[..32].copy_from_slice(&self.flash_header());
        let entries = [
            DirEntry::new(
                "uboot.boot",
                0x40,
                0x00,
                uboot_off as u32,
                bank.len() as u32,
                crc16(bank),
            ),
            DirEntry::new(
                "isd_config.ini",
                0x02,
                0xff,
                cfg_off as u32,
                cfg.len() as u32,
                crc16(&cfg),
            ),
            DirEntry::new("app_dir_head", 0x81, 0xff, AREA as u32, 0xffff_ffff, 0xffff).last(),
        ];
        for (i, e) in entries.iter().enumerate() {
            let at = 0x20 + i * ENTRY_LEN;
            out[at..at + ENTRY_LEN].copy_from_slice(&Enc::copy(0xffff, &e.to_bytes()));
        }
        // The bank header and the bank data are scrambled apart.
        let split = self.uboot.data_offset();
        out[uboot_off..uboot_off + split].copy_from_slice(&Enc::copy(0xffff, &bank[..split]));
        out[uboot_off + split..cfg_off].copy_from_slice(&Enc::copy(0xffff, &bank[split..]));
        out[cfg_off..cfg_off + cfg.len()].copy_from_slice(&cfg);
        Ok(out)
    }

    /// Builds the 32-byte flash header. The VID/PID are pre-scrambled so they
    /// read in the clear once the header is ENC-scrambled, and the CRC still
    /// holds (see kagaimiq's jlfs.md).
    fn flash_header(&self) -> [u8; 32] {
        let mut p = [0xff; 32];
        p[16..20].copy_from_slice(b"SH54");
        Enc::new(0xffff).apply(&mut p);
        p[2..4].copy_from_slice(&0x220u16.to_le_bytes()); // burner size
        p[8..12].copy_from_slice(&self.flash_size.to_le_bytes());
        p[12..16].copy_from_slice(&[0x0d, 0x01, 0xa4, 0x00]); // fs version, align, reserved, special
        let crc = crc16(&p[2..]);
        p[..2].copy_from_slice(&crc.to_le_bytes());
        Enc::new(0xffff).apply(&mut p);
        p
    }

    /// The `isd_config.ini` blob: a key-blob record, then
    /// `[size][key\0][value]` records.
    fn config_blob() -> Vec<u8> {
        // Key blob for 0xFFFF built from fixed bytes. The chip boots with either
        // this or the stock image's record.
        let first = std::array::from_fn(|i| 0x21 + i as u8);
        let mut n = 0u8;
        let blob = KeyBlob::new(0xffff, first, || {
            n += 1;
            n - 1
        });
        let mut out = vec![0x20];
        out.extend_from_slice(blob.bytes());
        out.push(0xff);
        out.push(5);
        out.extend_from_slice(b"SPI\x002_3_0");
        out.push(4);
        out.extend_from_slice(b"BOOT_TYPE\0");
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }
}

/// Result of [`FlashImage::check`].
pub struct CheckReport {
    pub lines: Vec<String>,
    pub mismatches: usize,
}

impl CheckReport {
    fn crc(&mut self, what: String, ok: bool) {
        self.mismatches += usize::from(!ok);
        self.lines
            .push(format!("{what} crc {}", if ok { "ok" } else { "MISMATCH" }));
    }
}

/// A flash image, from address 0.
pub struct FlashImage(pub Vec<u8>);

impl FlashImage {
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    fn plain_entry(&self, cipher: &AreaCipher, addr: usize) -> Option<DirEntry> {
        let raw = self.0.get(addr..addr + ENTRY_LEN)?;
        DirEntry::parse(&cipher.copy(raw, addr))
    }

    /// Recovers the flash key by trying every key until the first area entry
    /// is valid.
    pub fn find_key(&self) -> Result<u16> {
        ensure!(self.0.len() >= AREA + ENTRY_LEN, "image too short");
        (0..=0xffff)
            .find(|&k| {
                self.plain_entry(&AreaCipher(k), AREA)
                    .is_some_and(|e| e.name() == "app_area_head")
            })
            .ok_or_else(|| eyre!("no key gives an app_area_head at 0x1000"))
    }

    /// Walks the image and checks every CRC.
    pub fn check(&self) -> Result<CheckReport> {
        let img = &self.0;
        let mut r = CheckReport {
            lines: Vec::new(),
            mismatches: 0,
        };
        ensure!(img.len() >= AREA + ENTRY_LEN, "image too short");
        let hdr = Enc::copy(0xffff, &img[..32]);
        ensure!(
            crc16(&hdr[2..]) == u16_at(&hdr, 0),
            "flash header CRC mismatch"
        );
        r.lines
            .push(format!("flash header: size {:#x}", u32_at(&hdr, 8)));

        for i in 0..16 {
            let at = 0x20 + i * ENTRY_LEN;
            let Some(e) = DirEntry::parse(&Enc::copy(0xffff, &img[at..at + ENTRY_LEN])) else {
                break;
            };
            let what = format!(
                "  top  {:<15} off {:#07x} size {:#x}",
                e.name(),
                e.off,
                e.size
            );
            let (off, size) = (e.off as usize, e.size as usize);
            if e.name() == "uboot.boot" && off + size <= img.len() {
                let st = &img[off..off + size];
                let split = u32_at(&Enc::copy(0xffff, &st[..16]), 8) as usize;
                let mut plain = Enc::copy(0xffff, &st[..split.min(size)]);
                plain.extend(Enc::copy(0xffff, &st[split.min(size)..]));
                r.crc(what, crc16(&plain) == e.data_crc);
            } else if e.size != 0xffff_ffff && off + size <= img.len() {
                r.crc(what, crc16(&img[off..off + size]) == e.data_crc);
            } else {
                r.lines.push(what);
            }
            if e.last != 0 {
                break;
            }
        }

        let key = self.find_key()?;
        let cipher = AreaCipher(key);
        r.lines.push(format!("area key {key:#06x}"));
        let head = self.plain_entry(&cipher, AREA).unwrap();
        let area_end = (AREA + head.size as usize).min(img.len());
        let area = cipher.copy(&img[AREA..area_end], AREA);
        r.crc(
            format!("  area app_area_head size {:#x}", head.size),
            crc16(&area[ENTRY_LEN..]) == head.data_crc,
        );
        let mut at = ENTRY_LEN;
        while let Some(e) = area.get(at..at + ENTRY_LEN).and_then(DirEntry::parse) {
            let what = format!(
                "       {:<15} off {:#07x} size {:#x}",
                e.name(),
                e.off,
                e.size
            );
            let (off, size) = (e.off as usize, e.size as usize);
            if e.data_crc != 0xffff && off + size <= area.len() {
                r.crc(what, crc16(&area[off..off + size]) == e.data_crc);
            } else {
                r.lines.push(what);
            }
            at += ENTRY_LEN;
            if e.last != 0 {
                break;
            }
        }

        // Resource entries, such as audio, follow the application area.
        let mut at = AREA + head.size as usize;
        while let Some(e) = self.plain_entry(&cipher, at) {
            let end = (at + e.size as usize).min(img.len());
            let payload = cipher.copy(&img[at + ENTRY_LEN..end], at + ENTRY_LEN);
            r.crc(
                format!("  res  {:<15} at {at:#07x} size {:#x}", e.name(), e.size),
                crc16(&payload) == e.data_crc,
            );
            at += e.size as usize;
            if e.last != 0 {
                break;
            }
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_roundtrip() {
        let e = DirEntry::new("app.bin", 0x82, 0xff, 0x100, 0x3000, 0x1234).last();
        let p = DirEntry::parse(&e.to_bytes()).unwrap();
        assert_eq!(p.name(), "app.bin");
        assert_eq!((p.off, p.size, p.last), (0x100, 0x3000, 1));
    }

    #[test]
    fn area_cipher_is_an_involution() {
        let data: Vec<u8> = (0..200).map(|i| i as u8).collect();
        let c = AreaCipher(0x1e91);
        assert_eq!(c.copy(&c.copy(&data, 0x1013), 0x1013), data);
    }

    #[test]
    fn packed_image_checks_out() {
        // A fake bank image: 16-byte header, then the data.
        let mut bank = vec![0u8; 16];
        let data = [0x5a; 48];
        bank[2..4].copy_from_slice(&(data.len() as u16).to_le_bytes());
        bank[8..12].copy_from_slice(&16u32.to_le_bytes());
        bank[12..14].copy_from_slice(&crc16(&data).to_le_bytes());
        let crc = crc16(&bank[..14]);
        bank[14..16].copy_from_slice(&crc.to_le_bytes());
        bank.extend_from_slice(&data);
        let packer = Packer {
            key: 0x1e91,
            flash_size: 0x8_0000,
            uboot: Uboot::parse(&bank).unwrap(),
        };
        let img = packer.pack(&[0xaa; 0x1234]).unwrap();
        assert_eq!(img.bytes().len(), 0x3100);
        assert_eq!(img.find_key().unwrap(), 0x1e91);
        assert_eq!(img.check().unwrap().mismatches, 0);
    }
}
