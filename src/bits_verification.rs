//! Kani harnesses for the actual bit writer and reader.

use super::{BitReader, BitWriter};

#[kani::proof]
#[kani::unwind(66)]
fn unsigned_roundtrip() {
    let value: u64 = kani::any();
    let width: u32 = kani::any();
    kani::assume(width <= 64);
    let mut writer = BitWriter::new();
    writer.write_bits(value, width);
    let bytes = writer.finish();
    let mut reader = BitReader::new(&bytes);
    let actual = reader.read_bits(width);
    let expected = if width == 0 {
        0
    } else {
        value & (u64::MAX >> (64 - width))
    };
    assert_eq!(actual, expected);
    assert_eq!(reader.bits_consumed(), width as usize);
    assert!(!reader.overrun());
}

#[kani::proof]
#[kani::unwind(66)]
fn signed_roundtrip() {
    let value: i64 = kani::any();
    let width: u32 = kani::any();
    kani::assume((1..=64).contains(&width));
    let mut writer = BitWriter::new();
    writer.write_signed(value, width);
    let bytes = writer.finish();
    let mut reader = BitReader::new(&bytes);
    let actual = reader.read_signed(width);
    let shift = 64 - width;
    let expected = ((value as u64) << shift) as i64 >> shift;
    assert_eq!(actual, expected);
    assert_eq!(reader.bits_consumed(), width as usize);
    assert!(!reader.overrun());
}

// Model a short buffer, including the zero bytes returned by reads beyond it.
#[kani::proof]
#[kani::unwind(18)]
fn reader_exhaustion() {
    let bytes: [u8; 2] = kani::any();
    let len: usize = kani::any();
    let width: u32 = kani::any();
    kani::assume(len <= bytes.len());
    kani::assume(width <= 16);
    let mut reader = BitReader::new(&bytes[..len]);
    reader.read_bits(width);
    assert_eq!(reader.bits_consumed(), width as usize);
    assert_eq!(reader.overrun(), width as usize > len * 8);
    if width as usize >= len * 8 {
        assert!(!reader.read_bit());
        assert!(reader.overrun());
        assert_eq!(reader.bits_consumed(), width as usize + 1);
    }
}
