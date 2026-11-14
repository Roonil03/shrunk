//! Kani harnesses for the actual private codec helpers.

use super::{get_uvarint, put_uvarint, unzigzag, zigzag};

#[kani::proof]
fn zigzag_roundtrip() {
    let value: i64 = kani::any();
    assert_eq!(unzigzag(zigzag(value)), value);
}

#[kani::proof]
fn unzigzag_roundtrip() {
    let value: u64 = kani::any();
    assert_eq!(zigzag(unzigzag(value)), value);
}

#[kani::proof]
#[kani::unwind(11)]
fn varint_roundtrip() {
    let value: u64 = kani::any();
    let mut bytes = Vec::new();
    put_uvarint(&mut bytes, value);
    let mut pos = 0;
    assert_eq!(get_uvarint(&bytes, &mut pos), Ok(value));
    assert_eq!(pos, bytes.len());
}

// Every accepted prefix of an arbitrary, up-to-ten-byte input is the unique
// minimal encoding of the value returned by the actual parser.
#[kani::proof]
#[kani::unwind(11)]
fn varint_accepted_prefix_is_canonical() {
    let bytes: [u8; 10] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= bytes.len());
    let mut pos = 0;
    let result = get_uvarint(&bytes[..len], &mut pos);
    assert!(pos <= len);
    if let Ok(value) = result {
        let mut encoded = Vec::new();
        put_uvarint(&mut encoded, value);
        assert_eq!(encoded.as_slice(), &bytes[..pos]);
    }
}
