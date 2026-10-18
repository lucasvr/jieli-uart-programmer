//! Command link to the RAM loader.
//!
//! Commands are 16 bytes, `[0xFC][opcode][args...][nonce]`, prefixed with
//! their CRC16. Replies are 20 bytes, `[2 bytes][crc16][payload]`; the
//! payload echoes the command and has a status word at offset 2. Bulk data
//! follows as `[crc16][~crc16][data]`. After the key swap, all frames are
//! scrambled with CrcDecode. Multi-byte fields are big-endian.

use crate::dongle::Dongle;
use crate::jieli_image::{CrcDecode, XorShift, crc16};
use eyre::{Result, ensure};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEVICE_TYPE: u8 = 0xfc;
/// Status: the loader does not implement the opcode.
const NOT_IMPLEMENTED: u16 = 0x26;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum Op {
    ReadDeviceId = 0x01,
    SwapKey = 0x03,
    QueryBufferSize = 0x07,
    ReadChipKey = 0x09,
    EraseSector = 0x13,
    ReadEraseUnit = 0x15,
    WriteBuffer = 0x18,
    ReadBuffer = 0x19,
    Crc16 = 0x1b,
    DataDone = 0x20,
}

/// A 16-byte command.
struct Command([u8; 16]);

impl Command {
    fn new(op: Op) -> Self {
        let mut c = [0u8; 16];
        c[0] = DEVICE_TYPE;
        c[1] = op as u8;
        Self(c)
    }

    fn with_range(op: Op, addr: u32, len: u32) -> Self {
        let mut c = Self::new(op);
        c.0[2..6].copy_from_slice(&addr.to_be_bytes());
        c.0[6..10].copy_from_slice(&len.to_be_bytes());
        c
    }

    fn op(&self) -> u8 {
        self.0[1]
    }
}

/// A 16-byte reply payload and its status.
struct Reply {
    payload: [u8; 16],
    status: u16,
}

impl Reply {
    fn u16_at(&self, o: usize) -> u16 {
        u16::from_be_bytes([self.payload[o], self.payload[o + 1]])
    }

    fn u32_at(&self, o: usize) -> u32 {
        u32::from_be_bytes(self.payload[o..o + 4].try_into().unwrap())
    }
}

pub struct Link {
    dongle: Dongle,
    cipher: Option<CrcDecode>,
    rng: XorShift,
    retries: usize,
    timeout: Duration,
}

impl Link {
    pub fn new(dongle: Dongle, timeout: Duration) -> Self {
        Self {
            dongle,
            cipher: None,
            rng: XorShift::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0x9e37_79b9, |d| d.as_nanos() as u64),
            ),
            retries: 20,
            timeout,
        }
    }

    pub fn into_dongle(self) -> Dongle {
        self.dongle
    }

    fn scramble(&self, data: &mut [u8]) {
        if let Some(c) = &self.cipher {
            c.apply(data);
        }
    }

    fn frame(&mut self, mut cmd: [u8; 16]) -> Vec<u8> {
        if cmd[15] == 0 {
            cmd[15] = self.rng.byte();
        }
        let mut f = crc16(&cmd).to_be_bytes().to_vec();
        f.extend_from_slice(&cmd);
        self.scramble(&mut f);
        f
    }

    /// Reads a frame of `len` payload bytes, resynchronising on bad CRCs.
    fn read_frame(&mut self, len: usize) -> Result<Vec<u8>> {
        let need = len + 4;
        let mut buf = Vec::new();
        let deadline = Instant::now() + self.timeout;
        loop {
            while buf.len() < need {
                let left = deadline.saturating_duration_since(Instant::now());
                ensure!(!left.is_zero(), "timeout waiting for a {need}-byte frame");
                let mut tmp = vec![0u8; need - buf.len()];
                let n = self
                    .dongle
                    .read_exact(&mut tmp, left.min(Duration::from_millis(200)))?;
                buf.extend_from_slice(&tmp[..n]);
            }
            let mut f = buf[..need].to_vec();
            self.scramble(&mut f);
            // Replies carry the CRC at offset 2; bulk frames carry it at 0,
            // followed by its complement.
            let want = crc16(&f[4..]);
            let at0 = u16::from_be_bytes([f[0], f[1]]);
            let at2 = u16::from_be_bytes([f[2], f[3]]);
            if want == at2 || (want == at0 && at2 == !at0) {
                return Ok(f);
            }
            buf.remove(0);
        }
    }

    fn read_raw(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        let got = self.dongle.read_exact(&mut buf, self.timeout)?;
        ensure!(got == n, "timeout waiting for {n} bytes");
        self.scramble(&mut buf);
        Ok(buf)
    }

    fn exchange(&mut self, cmd: &Command, data: &[u8], bulk: Bulk) -> Result<(Reply, Vec<u8>)> {
        let mut f = self.frame(cmd.0);
        f.extend_from_slice(data);
        self.dongle.flush()?;
        self.dongle.write(&f)?;
        let r = self.read_frame(16)?;
        let payload: [u8; 16] = r[4..20].try_into().unwrap();
        ensure!(
            payload[..2] == cmd.0[..2],
            "reply does not echo the command"
        );
        let reply = Reply {
            payload,
            status: u16::from_be_bytes([payload[2], payload[3]]),
        };
        ensure!(
            reply.status == 0 || reply.status == NOT_IMPLEMENTED,
            "status {:#x} for opcode {:#04x}",
            reply.status,
            cmd.op()
        );
        let extra = match bulk {
            Bulk::None => Vec::new(),
            Bulk::Raw(n) => self.read_raw(n)?,
            Bulk::Framed(n) => {
                let mut f = self.read_frame(n)?;
                ensure!(f[0] == !f[2] && f[1] == !f[3], "bad bulk frame header");
                f.split_off(4)
            }
        };
        Ok((reply, extra))
    }

    /// Sends `cmd` followed by `data`, retrying on errors.
    fn send(&mut self, cmd: Command, data: &[u8], bulk: Bulk) -> Result<(Reply, Vec<u8>)> {
        let mut last = None;
        for attempt in 1..=self.retries {
            match self.exchange(&cmd, data, bulk) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    if self.dongle.verbose() {
                        eprintln!("[link] opcode {:#04x} attempt {attempt}: {e}", cmd.op());
                    }
                    last = Some(e);
                    if !data.is_empty() {
                        // Send padding to resynchronise the loader, as the vendor tool does.
                        self.dongle.write(&[0u8; 10])?;
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }
        Err(last.unwrap().wrap_err(format!(
            "opcode {:#04x} failed {} times",
            cmd.op(),
            self.retries
        )))
    }

    /// Negotiates the transfer key; must be the first command.
    pub fn swap_key(&mut self) -> Result<()> {
        let mut cmd = Command::new(Op::SwapKey);
        for b in &mut cmd.0[2..] {
            *b = self.rng.byte();
        }
        self.cipher = None;
        // The key is derived from the CRCs of the raw frames.
        let f = self.frame(cmd.0);
        self.dongle.flush()?;
        self.dongle.write(&f)?;
        let r = self.read_frame(16)?;
        ensure!(
            r[4..6] == cmd.0[..2],
            "key swap reply does not echo the command"
        );
        let tx = crc16(&f).to_be_bytes();
        let rx = crc16(&r).to_be_bytes();
        self.cipher = Some(CrcDecode::new([tx[0], tx[1], rx[0], rx[1]]));
        Ok(())
    }

    /// Returns the JEDEC flash id.
    pub fn read_device_id(&mut self) -> Result<u32> {
        Ok(self
            .send(Command::new(Op::ReadDeviceId), &[], Bulk::None)?
            .0
            .u32_at(6))
    }

    /// Returns `None` if the loader does not support the query, as on SH55.
    pub fn query_buffer_size(&mut self) -> Result<Option<u32>> {
        let (r, _) = self.send(Command::new(Op::QueryBufferSize), &[], Bulk::None)?;
        Ok((r.status != NOT_IMPLEMENTED).then(|| r.u32_at(4)))
    }

    pub fn read_erase_unit(&mut self) -> Result<u32> {
        Ok(self
            .send(Command::new(Op::ReadEraseUnit), &[], Bulk::None)?
            .0
            .u32_at(4))
    }

    pub fn read_chip_key(&mut self) -> Result<[u16; 3]> {
        let (r, _) = self.send(Command::new(Op::ReadChipKey), &[], Bulk::None)?;
        Ok([r.u16_at(4), r.u16_at(6), r.u16_at(8)])
    }

    pub fn erase_sector(&mut self, addr: u32) -> Result<()> {
        self.send(
            Command::with_range(Op::EraseSector, addr, 0),
            &[],
            Bulk::None,
        )?;
        Ok(())
    }

    pub fn data_done(&mut self) -> Result<()> {
        self.send(Command::new(Op::DataDone), &[], Bulk::None)?;
        Ok(())
    }

    /// CRC16 of flash, computed by the loader.
    pub fn flash_crc16(&mut self, addr: u32, len: u32) -> Result<u16> {
        let cmd = Command::with_range(Op::Crc16, addr, len);
        let (_, b) = self.send(cmd, &[], Bulk::Raw(4))?;
        let crc = u16::from_be_bytes([b[0], b[1]]);
        ensure!(
            u16::from_be_bytes([b[2], b[3]]) == !crc,
            "bad CRC16 reply {b:02x?}"
        );
        Ok(crc)
    }

    pub fn read_buffer(&mut self, addr: u32, len: u32) -> Result<Vec<u8>> {
        let cmd = Command::with_range(Op::ReadBuffer, addr, len);
        Ok(self.send(cmd, &[], Bulk::Framed(len as usize))?.1)
    }

    pub fn write_buffer(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        let mut cmd = Command::with_range(Op::WriteBuffer, addr, data.len() as u32);
        cmd.0[10..12].copy_from_slice(&crc16(data).to_be_bytes());
        self.send(cmd, data, Bulk::None)?;
        Ok(())
    }
}

/// Data that follows a reply.
#[derive(Clone, Copy)]
enum Bulk {
    None,
    /// `n` unframed bytes.
    Raw(usize),
    /// A `[crc][~crc][data]` frame of `n` data bytes.
    Framed(usize),
}
