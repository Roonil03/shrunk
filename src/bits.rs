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
