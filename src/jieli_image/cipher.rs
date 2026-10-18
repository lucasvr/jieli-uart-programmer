use super::crc::crc16_update;

/// The "ENC" keystream: an LFSR with poly 0x1021, one key byte per data byte.
pub struct Enc(u16);

impl Enc {
    pub fn new(key: u16) -> Self {
        Self(key)
    }

    /// XORs `data` with the keystream. Applying it twice gives the original.
    pub fn apply(&mut self, data: &mut [u8]) {
        for b in data {
            *b ^= self.0 as u8;
            self.0 = (self.0 << 1) ^ if self.0 & 0x8000 != 0 { 0x1021 } else { 0 };
        }
    }

    /// Returns a copy of `data` XORed with the keystream for `key`.
    pub fn copy(key: u16, data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        Self::new(key).apply(&mut out);
        out
    }
}

/// The "CrcDecode" keystream, seeded by a 4-byte key and a fixed string.
pub struct CrcDecode {
    seed: u16,
}

impl CrcDecode {
    /// "孟黎我爱你，玉林" in GB2312.
    const MAGIC: [u8; 16] = [
        0xc3, 0xcf, 0xc0, 0xe8, 0xce, 0xd2, 0xb0, 0xae, 0xc4, 0xe3, 0xa3, 0xac, 0xd3, 0xf1, 0xc1,
        0xd6,
    ];

    pub fn new(key: [u8; 4]) -> Self {
        let seed = crc16_update(u16::from_le_bytes([key[0], key[1]]), &key[2..4]);
        Self { seed }
    }

    /// XORs `data` with the keystream, which restarts for every call.
    pub fn apply(&self, data: &mut [u8]) {
        let mut crc = self.seed;
        for (i, b) in data.iter_mut().enumerate() {
            crc = crc16_update(crc, &Self::MAGIC[i % 16..i % 16 + 1]);
            *b ^= crc as u8;
        }
    }
}

/// The 32-byte encoding of a 16-bit chip key (`keyToByteArrayData`).
///
/// The byte sum of the first half sets a threshold; bit `i` of the key is 1
/// when `blob[16 + i] ^ blob[15 - i]` is below it.
pub struct KeyBlob([u8; 32]);

impl KeyBlob {
    /// Encodes `key`, with `first` as the first half and `rng` for the rest.
    pub fn new(key: u16, first: [u8; 16], mut rng: impl FnMut() -> u8) -> Self {
        let mut out = [0u8; 32];
        out[..16].copy_from_slice(&first);
        let v = Self::threshold(&first);
        for i in 0..16 {
            let d = if key & (1 << i) != 0 {
                rng() % v
            } else {
                (u16::from(rng()) % (0x100 - u16::from(v))) as u8 + v
            };
            out[16 + i] = out[15 - i] ^ d;
        }
        Self(out)
    }

    /// Encodes `key` using random bytes generated from `seed`.
    pub fn random(key: u16, seed: u64) -> Self {
        let mut rng = XorShift::new(seed);
        let first = std::array::from_fn(|_| rng.byte());
        Self::new(key, first, move || rng.byte())
    }

    fn threshold(first: &[u8]) -> u8 {
        match first.iter().fold(0u8, |a, &b| a.wrapping_add(b)) {
            ..=0x10 => 0x55,
            0xe0.. => 0xaa,
            v => v,
        }
    }

    pub fn key(&self) -> u16 {
        let v = Self::threshold(&self.0[..16]);
        (0..16)
            .filter(|&i| self.0[16 + i] ^ self.0[15 - i] < v)
            .fold(0, |k, i| k | 1 << i)
    }

    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// 64-bit xorshift PRNG for key blobs and link nonces. Not cryptographically
/// secure.
pub struct XorShift(u64);

impl XorShift {
    pub fn new(seed: u64) -> Self {
        // The state must not be zero.
        Self(seed | 1)
    }

    pub fn byte(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 24) as u8
    }
}

/// Derives the application area key by descrambling the first chip-key word
/// with CrcDecode (key 0xFFFFFFFF).
pub fn flash_key(chip_key: u16) -> u16 {
    let mut b = chip_key.to_le_bytes();
    CrcDecode::new([0xff; 4]).apply(&mut b);
    u16::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn enc_matches_reference() {
        let m = Enc::copy(0xffff, b"hello world 0123456789abcdef");
        assert_eq!(
            m,
            unhex("97baf373701e0b9782adc747fe8c69a43b2b0a4fe9ba462cff7d7640")
        );
    }

    #[test]
    fn crc_decode_matches_reference() {
        // From kagaimiq's jl-uboot-tool, key 0x78563412.
        let mut m = b"hello world 0123456789abcdef".to_vec();
        CrcDecode::new([0x12, 0x34, 0x56, 0x78]).apply(&mut m);
        assert_eq!(
            m,
            unhex("add1cb8ac2abaca556560e10046ff4158af3c9e73167ef7a06dcb15d")
        );
    }

    #[test]
    fn flash_key_of_ad152a4() {
        assert_eq!(flash_key(0xbabe), 0x1e91);
    }

    #[test]
    fn key_blob_roundtrip() {
        for (i, key) in [0x0000u16, 0xffff, 0x9847, 0x68af, 0x0001, 0x8000]
            .into_iter()
            .enumerate()
        {
            for seed in 0..50 {
                let blob = KeyBlob::random(key, seed * 7919 + i as u64);
                assert_eq!(blob.key(), key);
            }
        }
    }
}
