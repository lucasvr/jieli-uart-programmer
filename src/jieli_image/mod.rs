//! JieLi SH54/SH55 (AD15N, AD152A4) formats: checksums, keystreams, SDK
//! JLFV containers and flash images.

// The programmer uses only part of this API.
#![allow(dead_code, unused_imports)]

mod bytes;
mod cipher;
mod container;
mod crc;
mod flash_image;

pub use cipher::{CrcDecode, KeyBlob, XorShift, flash_key};
pub use container::Container;
pub use crc::{crc8, crc16};
pub use flash_image::{FlashImage, Packer, Uboot};
