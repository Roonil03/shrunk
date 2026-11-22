// Bit-level IO used by the Gorilla codec.
//
// Bits are written most-significant first. A trailing partial byte is padded
// with zeros. The block header carries the point count, so padding is never
// ambiguous.

pub struct BitWriter {
    buf: Vec<u8>,
    cur: u8,
    used: u8,
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl BitWriter {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            cur: 0,
            used: 0,
        }
    }

    #[inline]
    pub fn write_bit(&mut self, bit: bool) {
        self.cur = (self.cur << 1) | bit as u8;
        self.used += 1;
        if self.used == 8 {
            self.buf.push(self.cur);
            self.cur = 0;
            self.used = 0;
        }
    }

    /// Write the low `n` bits of `value`, most-significant bit first.
    #[inline]
    pub fn write_bits(&mut self, value: u64, n: u32) {
        debug_assert!(n <= 64);
        if n >= 8 {
            // Append the field to the low bits already waiting in cur.
            let mask = (1u128 << n) - 1;
            let packed = (u128::from(self.cur) << n) | (u128::from(value) & mask);
            let total = u32::from(self.used) + n;
            let bytes = (total / 8) as usize;
            self.used = (total % 8) as u8;
            let whole = ((packed >> self.used) as u64).to_be_bytes();
            self.buf.extend_from_slice(&whole[8 - bytes..]);
            self.cur = (packed as u8) & ((1 << self.used) - 1);
            return;
        }
        for i in (0..n).rev() {
            self.write_bit((value >> i) & 1 == 1);
        }
    }

    /// Write a two's complement signed value in `n` bits.
    #[inline]
    pub fn write_signed(&mut self, value: i64, n: u32) {
        self.write_bits(value as u64, n);
    }

    pub fn finish(mut self) -> Vec<u8> {
        if self.used > 0 {
            self.cur <<= 8 - self.used;
            self.buf.push(self.cur);
        }
        self.buf
    }
}

pub struct BitReader<'a> {
    pub(crate) buf: &'a [u8],
    bit: usize,
    overrun: bool,
}

impl<'a> BitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            bit: 0,
            overrun: false,
        }
    }

    #[inline]
    pub fn read_bit(&mut self) -> bool {
        let byte = self.bit / 8;
        if byte >= self.buf.len() {
            self.overrun = true;
            self.bit += 1;
            return false;
        }
        let shift = 7 - (self.bit % 8);
        self.bit += 1;
        (self.buf[byte] >> shift) & 1 == 1
    }

    #[inline]
    pub fn read_bits(&mut self, n: u32) -> u64 {
        debug_assert!(n <= 64);
        let offset = (self.bit % 8) as u32;
        if n >= 8 && n <= 64 - offset {
            if let Some(word) = self
                .buf
                .get(self.bit / 8..)
                .and_then(|tail| tail.first_chunk::<8>())
            {
                self.bit += n as usize;
                return (u64::from_be_bytes(*word) << offset) >> (64 - n);
            }
        }
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.read_bit() as u64;
        }
        v
    }

    #[inline]
    pub fn read_signed(&mut self, n: u32) -> i64 {
        debug_assert!(n > 0 && n <= 64);
        let v = self.read_bits(n);
        if n == 64 {
            v as i64
        } else {
            let sign = 1u64 << (n - 1);
            if v & sign != 0 {
                (v | !((1u64 << n) - 1)) as i64
            } else {
                v as i64
            }
        }
    }

    pub fn overrun(&self) -> bool {
        self.overrun
    }

    pub fn bits_consumed(&self) -> usize {
        self.bit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_fields_match_individual_bits() {
        let data: Vec<_> = (0..20u8).map(|i| i.wrapping_mul(79) ^ 171).collect();
        for len in 0..=data.len() {
            for start in 0..=len * 8 + 1 {
                for width in 0..=64 {
                    let mut field = BitReader::new(&data[..len]);
                    let mut bits = BitReader::new(&data[..len]);
                    for _ in 0..start {
                        field.read_bit();
                        bits.read_bit();
                    }
                    let expected = (0..width).fold(0, |v, _| (v << 1) | u64::from(bits.read_bit()));
                    assert_eq!(field.read_bits(width), expected);
                    assert_eq!(field.bits_consumed(), bits.bits_consumed());
                    assert_eq!(field.overrun(), bits.overrun());
                }
            }
        }
    }

    #[test]
    fn roundtrip_bits() {
        let mut w = BitWriter::new();
        w.write_bit(true);
        w.write_bit(false);
        w.write_bits(0b1011, 4);
        w.write_signed(-3, 5);
        w.write_bits(u64::MAX, 64);
        let bytes = w.finish();

        let mut r = BitReader::new(&bytes);
        assert!(r.read_bit());
        assert!(!r.read_bit());
        assert_eq!(r.read_bits(4), 0b1011);
        assert_eq!(r.read_signed(5), -3);
        assert_eq!(r.read_bits(64), u64::MAX);
        assert!(!r.overrun());
    }

    #[test]
    fn write_fields_match_individual_bits() {
        for offset in 0..8 {
            for width in 0..=64 {
                for value in [0, u64::MAX, 0xa376_b59c_5e21_70da] {
                    let mut field = BitWriter::new();
                    let mut bits = BitWriter::new();
                    for _ in 0..offset {
                        field.write_bit(true);
                        bits.write_bit(true);
                    }
                    field.write_bits(value, width);
                    for shift in (0..width).rev() {
                        bits.write_bit((value >> shift) & 1 == 1);
                    }
                    field.write_bit(true);
                    bits.write_bit(true);
                    assert_eq!(field.finish(), bits.finish());
                }
            }
        }
    }

    #[test]
    fn signs() {
        for v in [-2048i64, -100, -1, 0, 1, 64, 2047] {
            let mut w = BitWriter::new();
            w.write_signed(v, 12);
            let b = w.finish();
            let mut r = BitReader::new(&b);
            assert_eq!(r.read_signed(12), v);
        }
    }

    #[test]
    fn overrun_is_reported() {
        let bytes = [0u8];
        let mut r = BitReader::new(&bytes);
        r.read_bits(9);
        assert!(r.overrun());
    }
}

#[cfg(kani)]
#[path = "bits_verification.rs"]
mod verification;
