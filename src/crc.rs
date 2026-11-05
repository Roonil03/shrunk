//! CRC-32 (IEEE 802.3) used by the on-disk format.
//!
//! The implementation is intentionally small and dependency-free.

pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
