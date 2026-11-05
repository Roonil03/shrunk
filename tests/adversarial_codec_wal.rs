use std::fs;

use shrunk::{Codec, Db};

#[test]
fn rle_rejects_a_run_length_that_overflows_the_remaining_point_bound() {
    // The first run emits one point; the second claims u64::MAX more while
    // the caller supplied a count of 2. This used to overflow
    // `out.len() + len as usize` before rejecting it.
    let mut encoded = vec![2, 0, 2, 1]; // two runs; first starts at 0, step 1, len 1
    encoded.extend_from_slice(&42.0f64.to_bits().to_le_bytes());
    encoded.extend_from_slice(&[2, 2]); // second start delta 1, step 1
    encoded.extend_from_slice(&[0xff; 9]);
    encoded.push(0x01); // u64::MAX in unsigned varint form
    encoded.extend_from_slice(&42.0f64.to_bits().to_le_bytes());

    let error = Codec::Rle.decode(&encoded, 2).unwrap_err();
    assert!(
        error.contains("runs exceed point count")
            || error.contains("run length exceeds platform limits"),
        "{error}"
    );
}

#[test]
fn wal_header_uses_the_persisted_magic() {
    let dir = std::env::temp_dir().join(format!(
        "shrunk-adversarial-wal-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.shrunk");
    let wal_path = dir.join("db.shrunk.wal");

    let db = Db::open_rw_with_block_size(&path, 64).unwrap();
    let wal = fs::read(&wal_path).unwrap();
    assert_eq!(&wal[..8], b"SHRUNKWL");
    drop(db);

    fs::remove_dir_all(dir).unwrap();
}
