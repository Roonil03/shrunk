use shrunk::{Codec, Db, Point, Query};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};

fn rng(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

#[test]
fn codecs_roundtrip_adversarial_ieee_and_wrapping_timestamps() {
    let mut state = 0x148d_76a3_c204_9b5fu64;
    let values = [
        0,
        1,
        1u64 << 63,
        0x7ff0_0000_0000_0000,
        0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_0001,
        0x7ff0_0000_0000_0001,
        0x000f_ffff_ffff_ffff,
        0x0010_0000_0000_0000,
        0x3ff0_0000_0000_0000,
        0xbff0_0000_0000_0000,
    ];
    for trial in 0..300 {
        let n = 1 + (rng(&mut state) as usize % 300);
        let mut points = Vec::with_capacity(n);
        for i in 0..n {
            let ts = match i % 7 {
                0 => i64::MIN.wrapping_add(rng(&mut state) as i64),
                1 => i64::MAX.wrapping_sub(rng(&mut state) as i64),
                _ => rng(&mut state) as i64,
            };
            let bits = if i % 3 == 0 {
                values[(rng(&mut state) as usize) % values.len()]
            } else {
                rng(&mut state)
            };
            points.push(Point {
                ts,
                value: f64::from_bits(bits),
            });
        }
        for codec in Codec::ALL {
            let Some(encoded) = codec.encode(&points) else {
                continue;
            };
            let decoded = codec
                .decode(&encoded, points.len())
                .unwrap_or_else(|e| panic!("{codec:?} trial {trial} failed to decode: {e}"));
            assert_eq!(decoded.len(), points.len(), "{codec:?} trial {trial}");
            for (idx, (a, b)) in points.iter().zip(&decoded).enumerate() {
                assert_eq!(b.ts, a.ts, "{codec:?} trial {trial} timestamp {idx}");
                assert_eq!(
                    b.value.to_bits(),
                    a.value.to_bits(),
                    "{codec:?} trial {trial} value {idx}"
                );
            }
        }
    }
}

#[test]
fn codecs_cover_empty_singleton_and_alternating_timestamp_extremes() {
    for codec in Codec::ALL {
        assert!(
            codec.encode(&[]).is_none(),
            "{codec:?} encoded an empty slice"
        );
        assert!(
            codec.decode(&[0xaa], 0).unwrap().is_empty(),
            "{codec:?} mishandled count zero"
        );
    }

    let singleton = [Point {
        ts: i64::MIN,
        value: f64::from_bits(0x7ff8_0000_0000_002a),
    }];
    let alternating = [
        Point {
            ts: i64::MIN,
            value: f64::from_bits(0x8000_0000_0000_0000),
        },
        Point {
            ts: i64::MAX,
            value: 0.0,
        },
        Point {
            ts: i64::MIN + 1,
            value: f64::INFINITY,
        },
        Point {
            ts: i64::MAX - 1,
            value: f64::NEG_INFINITY,
        },
        Point {
            ts: 0,
            value: f64::from_bits(1),
        },
    ];

    for (points, label) in [
        (&singleton[..], "singleton"),
        (&alternating[..], "alternating extremes"),
    ] {
        for codec in Codec::ALL {
            let Some(bytes) = codec.encode(points) else {
                continue;
            };
            let decoded = codec
                .decode(&bytes, points.len())
                .unwrap_or_else(|err| panic!("{codec:?} failed {label} roundtrip: {err}"));
            for (index, (expected, actual)) in points.iter().zip(&decoded).enumerate() {
                assert_eq!(
                    actual.ts, expected.ts,
                    "{codec:?} {label} timestamp {index}"
                );
                assert_eq!(
                    actual.value.to_bits(),
                    expected.value.to_bits(),
                    "{codec:?} {label} value {index}"
                );
            }
        }
    }
}

#[test]
fn every_codec_returns_err_for_an_empty_nonzero_count_payload() {
    for codec in Codec::ALL {
        assert!(
            codec.decode(&[], 1).is_err(),
            "{codec:?} accepted a missing payload"
        );
    }
}

#[test]
fn decoders_return_errors_instead_of_panicking_on_arbitrary_bytes() {
    let mut state = 0x7792_4a11_83b0_c5e7u64;
    for trial in 0..100_000 {
        let len = (rng(&mut state) as usize) % 48;
        let mut bytes = Vec::with_capacity(len);
        while bytes.len() < len {
            bytes.push(rng(&mut state) as u8);
        }
        let count = 1 + (rng(&mut state) as usize % 24);
        for codec in Codec::ALL {
            let result = catch_unwind(AssertUnwindSafe(|| codec.decode(&bytes, count)));
            assert!(
                result.is_ok(),
                "{codec:?} panicked on bytes {bytes:02x?}, count {count}, trial {trial}"
            );
        }
    }
}

#[test]
fn rle_rejects_a_run_length_that_overflows_the_output_count() {
    // Two runs: the first emits one point, then the second advertises
    // u64::MAX points. The decoder must reject it before adding the length to
    // the number of points already emitted.
    let mut bytes = vec![2, 0, 0, 1];
    bytes.extend_from_slice(&0.0f64.to_bits().to_le_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(&[0xff; 9]);
    bytes.push(0x01);
    bytes.extend_from_slice(&0.0f64.to_bits().to_le_bytes());
    let result = catch_unwind(AssertUnwindSafe(|| Codec::Rle.decode(&bytes, 3)));
    let decoded = result.expect("RLE decoder panicked on an oversized run length");
    assert!(decoded.is_err(), "RLE accepted an oversized run length");
}

#[test]
fn checksum_valid_snapshot_with_oversized_rle_run_does_not_panic_on_query() {
    let base = std::env::temp_dir().join(format!("shrunk-adversarial-rle-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    let path = base.join("db");

    let mut payload = vec![2, 0, 0, 1];
    payload.extend_from_slice(&0.0f64.to_bits().to_le_bytes());
    payload.extend_from_slice(&[0, 0]);
    payload.extend_from_slice(&[0xff; 9]);
    payload.push(0x01);
    payload.extend_from_slice(&0.0f64.to_bits().to_le_bytes());

    let mut file = Vec::new();
    file.extend_from_slice(&shrunk::FORMAT_MAGIC);
    file.extend_from_slice(&shrunk::FORMAT_VERSION.to_le_bytes());
    file.extend_from_slice(&0u16.to_le_bytes());
    file.extend_from_slice(&3u32.to_le_bytes());
    file.extend_from_slice(&1u32.to_le_bytes());
    file.extend_from_slice(&1u32.to_le_bytes());
    file.extend_from_slice(b"m");
    file.extend_from_slice(&1u32.to_le_bytes());
    let header_start = file.len();
    file.push(Codec::Rle as u8);
    file.push(1); // finite sum present
    file.extend_from_slice(&3u32.to_le_bytes());
    file.extend_from_slice(&0i64.to_le_bytes());
    file.extend_from_slice(&2i64.to_le_bytes());
    file.extend_from_slice(&0.0f64.to_le_bytes());
    file.extend_from_slice(&0.0f64.to_le_bytes());
    file.extend_from_slice(&0.0f64.to_le_bytes());
    file.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    let header_crc = crc32(&file[header_start..]);
    file.extend_from_slice(&header_crc.to_le_bytes());
    file.extend_from_slice(&payload);
    file.extend_from_slice(&crc32(&payload).to_le_bytes());
    fs::write(&path, file).unwrap();

    let db = Db::open(&path).expect("snapshot metadata and checksums are valid");
    let result = catch_unwind(AssertUnwindSafe(|| db.query(&Query::new("m", 0, 2))));
    assert!(
        result.is_ok(),
        "query panicked on checksum-valid malformed RLE payload"
    );
    let _ = fs::remove_dir_all(base);
}

#[test]
fn snapshot_header_length_flips_and_truncations_return_errors_without_panics() {
    let base =
        std::env::temp_dir().join(format!("shrunk-adversarial-parse-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    let path = base.join("db");
    let mut db = Db::with_block_size(3);
    for i in 0..3 {
        db.write("m", i, 1.0).unwrap();
    }
    db.save(&path).unwrap();
    let valid = fs::read(&path).unwrap();

    let mut malformed = Vec::new();
    for cut in [0, 7, 19, 28, 29, 30, 82, valid.len() - 1] {
        malformed.push(valid[..cut].to_vec());
    }
    // First block begins at byte 29; its payload length is at +46. Recompute
    // the header checksum so the parser reaches the oversized length field.
    let mut length_flip = valid.clone();
    let block = 29;
    length_flip[block + 46..block + 50].copy_from_slice(&u32::MAX.to_le_bytes());
    let header_crc = crc32(&length_flip[block..block + 50]);
    length_flip[block + 50..block + 54].copy_from_slice(&header_crc.to_le_bytes());
    malformed.push(length_flip);
    // A single bit flip in the header checksum itself must also fail cleanly.
    let mut crc_flip = valid;
    crc_flip[block + 50] ^= 0x80;
    malformed.push(crc_flip);

    for (i, bytes) in malformed.iter().enumerate() {
        fs::write(&path, bytes).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| Db::open(&path)));
        let opened =
            result.unwrap_or_else(|_| panic!("Db::open panicked on snapshot mutation {i}"));
        assert!(opened.is_err(), "Db::open accepted snapshot mutation {i}");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn wal_corruption_and_replay_do_not_panic() {
    let base = std::env::temp_dir().join(format!("shrunk-adversarial-wal-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    let path = base.join("db");
    {
        let mut db = Db::open_rw_with_block_size(&path, 16).unwrap();
        for i in 0..64 {
            db.write("m", i, (i * 3) as f64).unwrap();
        }
    }
    let wal = base.join("db.wal");
    let original = fs::read(&wal).unwrap();
    let mut state = 0x6e21_00a5_9f44_7813u64;
    for trial in 0..500 {
        let mut bytes = original.clone();
        let edits = 1 + (rng(&mut state) % 4) as usize;
        for _ in 0..edits {
            if !bytes.is_empty() {
                let at = (rng(&mut state) as usize) % bytes.len();
                bytes[at] ^= rng(&mut state) as u8;
            }
        }
        fs::write(&wal, bytes).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| Db::open(&path)));
        assert!(result.is_ok(), "Db::open panicked on WAL mutation {trial}");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn newly_written_database_keeps_bit_patterns_after_checkpoint() {
    let base = std::env::temp_dir().join(format!(
        "shrunk-adversarial-snapshot-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    let path = base.join("db");
    let mut db = Db::open_rw_with_block_size(&path, 128).unwrap();
    for i in 0..128 {
        // Keep timestamps increasing, while exercising signed zeros, NaNs,
        // infinities, subnormals, and arbitrary mantissas through the selected codec.
        let bits = match i {
            0 => 0x8000_0000_0000_0000,
            1 => 0x7ff8_0000_0000_002a,
            2 => 0x7ff0_0000_0000_0000,
            3 => 0xfff0_0000_0000_0000,
            4 => 1,
            _ => (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15),
        };
        db.write("m", i as i64, f64::from_bits(bits)).unwrap();
    }
    db.checkpoint().unwrap();
    let reopened = Db::open(&path).unwrap();
    let points = reopened
        .query(&Query::new("m", i64::MIN, i64::MAX))
        .unwrap()
        .points;
    assert_eq!(points.len(), 128);
    for (i, point) in points.iter().enumerate() {
        let expected = match i {
            0 => 0x8000_0000_0000_0000,
            1 => 0x7ff8_0000_0000_002a,
            2 => 0x7ff0_0000_0000_0000,
            3 => 0xfff0_0000_0000_0000,
            4 => 1,
            _ => (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15),
        };
        assert_eq!(point.value.to_bits(), expected, "value bit pattern {i}");
    }
    let _ = fs::remove_dir_all(base);
}
