/// CRC-16/XMODEM (poly 0x1021), starting from `crc`.
pub fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// CRC-16/XMODEM (poly 0x1021, init 0).
pub fn crc16(data: &[u8]) -> u16 {
    crc16_update(0, data)
}

/// CRC-8 with the reflected poly 0x8C, init 0.
pub fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x8c
            } else {
                crc >> 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_values() {
        assert_eq!(crc16(b"123456789"), 0x31c3);
        assert_eq!(crc8(b"123456789"), 0xa1);
    }
}
