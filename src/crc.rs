//! CRC-32 (IEEE 802.3) used by the on-disk format.
//!
//! The implementation is intentionally small and dependency-free. Four lookup tables
//! process four bytes of the reflected IEEE polynomial at a time.

const fn crc32_table() -> [[u32; 256]; 4] {
    let mut table = [[0u32; 256]; 4];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[0][i] = crc;
        i += 1;
    }
    // Each subsequent table advances the previous CRC by one zero byte.
    let mut slice = 1;
    while slice < 4 {
        let mut i = 0;
        while i < 256 {
            let crc = table[slice - 1][i];
            table[slice][i] = (crc >> 8) ^ table[0][crc as u8 as usize];
            i += 1;
        }
        slice += 1;
    }
    table
}

static CRC32_TABLE: [[u32; 256]; 4] = crc32_table();

pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    let (words, tail) = data.as_chunks::<4>();
    for word in words {
        let [a, b, c, d] = (crc ^ u32::from_le_bytes(*word)).to_le_bytes();
        crc = CRC32_TABLE[3][a as usize]
            ^ CRC32_TABLE[2][b as usize]
            ^ CRC32_TABLE[1][c as usize]
            ^ CRC32_TABLE[0][d as usize];
    }
    for &b in tail {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = CRC32_TABLE[0][idx] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_bytewise_crc_across_word_boundaries() {
        let bytes: Vec<_> = (0..=255u8).collect();
        for start in 0..4 {
            let mut crc = u32::MAX;
            assert_eq!(crc32(&[]), !crc);
            for end in start..bytes.len() {
                let idx = (crc as u8 ^ bytes[end]) as usize;
                crc = CRC32_TABLE[0][idx] ^ (crc >> 8);
                assert_eq!(crc32(&bytes[start..=end]), !crc);
            }
        }
    }

    #[test]
    fn known_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
