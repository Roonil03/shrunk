use std::fs;

use shrunk::{Agg, Db, Point, Predicate, Query};

fn series(n: usize, start: i64, step: i64) -> Vec<Point> {
    (0..n)
        .map(|i| Point {
            ts: start + i as i64 * step,
            value: ((i * 7) % 101) as f64 - 50.0,
        })
        .collect()
}

#[test]
fn scan_respects_time_and_value_filters() {
    let mut db = Db::with_block_size(64);
    let pts = series(1000, 1_000, 10);
    db.write_many("m", &pts).unwrap();

    let all = db.query(&Query::new("m", i64::MIN, i64::MAX)).unwrap();
    assert_eq!(all.points, pts);
    assert_eq!(all.stats.blocks_decoded, all.stats.blocks_total);

    // Inclusive bounds: points 100..=199.
    let window = db
        .query(&Query::new("m", 1_000 + 100 * 10, 1_000 + 199 * 10))
        .unwrap();
    assert_eq!(window.points.len(), 100);
    assert_eq!(window.points[0], pts[100]);
    assert_eq!(window.points[99], pts[199]);
    assert!(window.stats.blocks_skipped_time >= 8);

    let hits: Vec<Point> = pts
        .iter()
        .copied()
        .filter(|p| p.value > 40.0 && p.ts >= 2_000 && p.ts <= 7_000)
        .collect();
    let filtered = db
        .query(&Query::new("m", 2_000, 7_000).filter(Predicate::Gt(40.0)))
        .unwrap();
    assert_eq!(filtered.points, hits);

    // No value reaches 1000, so every block should be rejected by its value
    // bounds before any payload is decoded.
    let impossible = db
        .query(&Query::new("m", i64::MIN, i64::MAX).filter(Predicate::Gt(1000.0)))
        .unwrap();
    assert!(impossible.points.is_empty());
    assert_eq!(impossible.stats.blocks_decoded, 0);
    assert_eq!(
        impossible.stats.blocks_skipped_value,
        impossible.stats.blocks_total
    );

    let limited = db
        .query(&Query::new("m", i64::MIN, i64::MAX).limit(7))
        .unwrap();
    assert_eq!(limited.points, pts[..7]);
    assert_eq!(limited.stats.blocks_skipped_time, 0);
    assert!(limited.stats.blocks_skipped_limit > 0);
}

#[test]
fn aggregates_match_a_naive_fold() {
    let mut db = Db::with_block_size(97);
    let pts = series(2000, 10_000, 5);
    db.write_many("m", &pts).unwrap();

    let start = 12_000;
    let end = 17_000;
    let selected: Vec<&Point> = pts
        .iter()
        .filter(|p| p.ts >= start && p.ts <= end)
        .collect();

    let count = db.aggregate("m", start, end, Agg::Count, None).unwrap();
    assert_eq!(count.value, Some(selected.len() as f64));

    let sum = db.aggregate("m", start, end, Agg::Sum, None).unwrap();
    let naive_sum: f64 = selected.iter().map(|p| p.value).sum();
    assert!((sum.value.unwrap() - naive_sum).abs() < 1e-9);

    let min = db.aggregate("m", start, end, Agg::Min, None).unwrap();
    let naive_min = selected
        .iter()
        .map(|p| p.value)
        .fold(f64::INFINITY, f64::min);
    assert_eq!(min.value, Some(naive_min));

    let max = db.aggregate("m", start, end, Agg::Max, None).unwrap();
    let naive_max = selected
        .iter()
        .map(|p| p.value)
        .fold(f64::NEG_INFINITY, f64::max);
    assert_eq!(max.value, Some(naive_max));

    let avg = db.aggregate("m", start, end, Agg::Avg, None).unwrap();
    assert!((avg.value.unwrap() - naive_sum / selected.len() as f64).abs() < 1e-9);

    // Full range with no predicate should be answered without decoding.
    let full = db
        .aggregate("m", i64::MIN, i64::MAX, Agg::Avg, None)
        .unwrap();
    assert_eq!(full.stats.blocks_decoded, 0);
    assert_eq!(full.stats.fast_blocks, full.stats.blocks_total);

    // Value predicate disables the fast path and still gives the right answer.
    let gated = db
        .aggregate("m", start, end, Agg::Count, Some(Predicate::Gt(20.0)))
        .unwrap();
    let naive = selected.iter().filter(|p| p.value > 20.0).count();
    assert_eq!(gated.value, Some(naive as f64));

    let impossible = db
        .aggregate("m", start, end, Agg::Count, Some(Predicate::Gt(1000.0)))
        .unwrap();
    assert_eq!(impossible.value, None);
    assert_eq!(
        impossible.stats.blocks_skipped_value,
        impossible.stats.blocks_visited
    );
}

#[test]
fn compression_picks_a_codec_per_block() {
    let mut db = Db::with_block_size(256);
    for i in 0..256 {
        db.write("flat", i * 1000, 42.0).unwrap();
        db.write("ramp", i * 1000, i as f64).unwrap();
        db.write("noisy", i * 1000, (i as f64 * 1.234567).sin() * 1000.0)
            .unwrap();
    }
    let summaries = db.summaries();
    let flat = summaries.iter().find(|s| s.name == "flat").unwrap();
    assert_eq!(flat.codecs.len(), 1);
    assert_eq!(flat.codecs[0].codec, "rle");
    assert!(flat.ratio() > 10.0, "flat ratio was {}", flat.ratio());

    let noisy = summaries.iter().find(|s| s.name == "noisy").unwrap();
    assert!(noisy.ratio() > 1.0, "noisy ratio was {}", noisy.ratio());
    assert!(noisy.stored_bytes < noisy.raw_bytes as usize);

    let ramp = summaries.iter().find(|s| s.name == "ramp").unwrap();
    assert!(ramp.ratio() > 2.0, "ramp ratio was {}", ramp.ratio());
}

#[test]
fn empty_blocks_are_never_produced_and_empty_ranges_work() {
    let mut db = Db::new();
    let res = db.query(&Query::new("missing", 0, 10)).unwrap_err();
    assert!(format!("{res}").contains("unknown series"));

    db.write("m", 5, 1.0).unwrap();
    let res = db.query(&Query::new("m", 10, 20)).unwrap();
    assert!(res.points.is_empty());
    assert_eq!(res.stats.blocks_visited, 0);

    let agg = db.aggregate("m", 10, 20, Agg::Avg, None).unwrap();
    assert_eq!(agg.value, None);
}

#[test]
fn save_and_open_roundtrip() {
    let dir = std::env::temp_dir().join(format!("shrunk-it-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("db.shrunk");
    let _ = fs::remove_file(&path);

    let mut db = Db::with_block_size(128);
    db.write_many("cpu", &series(1000, 1_000, 10)).unwrap();
    db.write_many("flat", &{
        (0..500)
            .map(|i| Point {
                ts: 1_000 + i * 10,
                value: 7.0,
            })
            .collect::<Vec<_>>()
    })
    .unwrap();
    db.flush_all();
    let before = db.summaries();
    db.save(&path).unwrap();

    assert!(fs::metadata(&path).unwrap().len() > 0);
    let reopened = Db::open(&path).unwrap();
    assert_eq!(reopened.block_size(), 128);
    assert_eq!(reopened.point_count(), 1500);

    let a = db.query(&Query::new("cpu", i64::MIN, i64::MAX)).unwrap();
    let b = reopened
        .query(&Query::new("cpu", i64::MIN, i64::MAX))
        .unwrap();
    assert_eq!(a.points, b.points);

    let after = reopened.summaries();
    assert_eq!(before.len(), after.len());
    for (x, y) in before.iter().zip(after.iter()) {
        assert_eq!(x.name, y.name);
        assert_eq!(x.points, y.points);
        assert_eq!(x.blocks, y.blocks);
        assert_eq!(x.stored_bytes, y.stored_bytes);
    }

    // Appending after reopen keeps working.
    let mut reopened = reopened;
    let last = reopened
        .query(&Query::new("cpu", i64::MIN, i64::MAX))
        .unwrap()
        .points
        .last()
        .copied()
        .unwrap();
    reopened.write("cpu", last.ts + 10, 123.0).unwrap();
    reopened.save(&path).unwrap();
    let reopened2 = Db::open(&path).unwrap();
    assert_eq!(reopened2.point_count(), 1501);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_payload_is_rejected() {
    let dir = std::env::temp_dir().join(format!("shrunk-corrupt-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("db.shrunk");
    let _ = fs::remove_file(&path);

    let mut db = Db::with_block_size(64);
    db.write_many("m", &series(500, 0, 1)).unwrap();
    db.save(&path).unwrap();

    let mut bytes = fs::read(&path).unwrap();
    let n = bytes.len();
    bytes[n - 10] ^= 0xFF;
    fs::write(&path, &bytes).unwrap();

    // v3 opens from metadata only; the payload checksum is verified lazily.
    let db = Db::open(&path).expect("metadata is intact");
    let err = db.query(&Query::new("m", i64::MIN, i64::MAX)).unwrap_err();
    assert!(format!("{err}").contains("checksum"), "got: {err}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn duplicate_and_out_of_order_writes_are_rejected_atomically() {
    let mut db = Db::with_block_size(4);
    let base = series(4, 0, 1);
    db.write_many("m", &base).unwrap();
    assert_eq!(db.point_count(), 4);

    // A duplicate of the current last timestamp is rejected.
    let err = db.write("m", 3, 99.0).unwrap_err();
    assert!(
        matches!(err, shrunk::Error::Duplicate { ts: 3, .. }),
        "{err}"
    );

    // A bad point later in a batch must not partially apply the batch.
    let bad = vec![
        Point { ts: 4, value: 1.0 },
        Point { ts: 5, value: 2.0 },
        Point { ts: 5, value: 3.0 },
    ];
    let err = db.write_many("m", &bad).unwrap_err();
    assert!(
        matches!(err, shrunk::Error::Duplicate { ts: 5, .. }),
        "{err}"
    );
    assert_eq!(db.point_count(), 4);
    assert!(db
        .query(&Query::new("m", i64::MIN, i64::MAX))
        .unwrap()
        .points
        .iter()
        .all(|p| p.ts < 4));

    // Out-of-order inside a batch is also atomic.
    let bad = vec![Point { ts: 4, value: 1.0 }, Point { ts: 2, value: 2.0 }];
    let err = db.write_many("m", &bad).unwrap_err();
    assert!(matches!(err, shrunk::Error::OutOfOrder { .. }), "{err}");
    assert_eq!(db.point_count(), 4);

    // Empty batches are no-ops.
    db.write_many("m", &[]).unwrap();
    assert_eq!(db.point_count(), 4);
}

#[test]
fn v1_fixture_opens_and_migrates_to_current_format() {
    let dir = std::env::temp_dir().join(format!("shrunk-migrate-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("v1.shrunk");
    let fixture = include_bytes!("fixtures/v1.shrunk");
    fs::write(&path, fixture).unwrap();

    let mut db = Db::open(&path).unwrap();
    assert_eq!(db.format_version(), 1);
    assert_eq!(db.point_count(), 300);
    let before = db
        .query(&Query::new("fixture", i64::MIN, i64::MAX))
        .unwrap()
        .points;
    assert_eq!(before.len(), 300);

    db.save(&path).unwrap();
    assert_eq!(&fs::read(&path).unwrap()[..8], &shrunk::FORMAT_MAGIC);
    let reopened = Db::open(&path).unwrap();
    assert_eq!(reopened.format_version(), 3);
    let after = reopened
        .query(&Query::new("fixture", i64::MIN, i64::MAX))
        .unwrap()
        .points;
    assert_eq!(before, after);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn v2_fixture_opens_and_migrates_to_current_format() {
    let dir = std::env::temp_dir().join(format!("shrunk-migrate2-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("v2.shrunk");
    fs::write(&path, include_bytes!("fixtures/v2.shrunk")).unwrap();

    let mut db = Db::open(&path).unwrap();
    assert_eq!(db.format_version(), 2);
    assert_eq!(db.point_count(), 300);
    let before = db
        .query(&Query::new("fixture", i64::MIN, i64::MAX))
        .unwrap()
        .points;
    assert_eq!(before.len(), 300);

    db.save(&path).unwrap();
    assert_eq!(&fs::read(&path).unwrap()[..8], &shrunk::FORMAT_MAGIC);
    let reopened = Db::open(&path).unwrap();
    assert_eq!(reopened.format_version(), 3);
    let after = reopened
        .query(&Query::new("fixture", i64::MIN, i64::MAX))
        .unwrap()
        .points;
    assert_eq!(before, after);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn v3_header_and_metadata_corruption_are_rejected() {
    let dir = std::env::temp_dir().join(format!("shrunk-header-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("db.shrunk");

    let mut db = Db::with_block_size(2);
    db.write_many("m", &series(4, 100, 10)).unwrap();
    db.save(&path).unwrap();
    let good = fs::read(&path).unwrap();
    assert_eq!(&good[..8], &shrunk::FORMAT_MAGIC);

    // v3 header: magic(8) version(2) flags(2) block_size(4) series_count(4)
    // series: name_len(4) name(1) block_count(4); block starts at 29.
    let block_start = 29usize;

    let check = |bytes: &[u8], needle: &str| {
        fs::write(&path, bytes).unwrap();
        let err = Db::open(&path).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(needle), "expected {needle:?}, got {msg}");
    };

    // v3 block metadata has its own checksum, checked without payloads.
    let mut b = good.clone();
    b[block_start + 2] ^= 0x01; // count
    check(&b, "header checksum");

    let mut b = good.clone();
    b[block_start + 6] ^= 0x01; // t_min
    check(&b, "header checksum");

    let mut b = good.clone();
    b[block_start + 46] = 0xFF; // payload_len high byte
    b[block_start + 47] = 0xFF;
    b[block_start + 48] = 0xFF;
    b[block_start + 49] = 0xFF;
    check(&b, "header checksum");

    // Structural limits are checked before allocation.
    let mut b = good.clone();
    b[12..16].copy_from_slice(&0u32.to_le_bytes()); // block_size
    check(&b, "block size");

    let mut b = good.clone();
    b[16..20].copy_from_slice(&u32::MAX.to_le_bytes()); // series_count
    check(&b, "series count");

    // Truncation and trailing bytes.
    check(&good[..good.len() - 1], "truncated");
    let mut b = good.clone();
    b.push(0);
    check(&b, "trailing bytes");

    // v1 has no header CRC, so metadata and limits must be validated directly.
    let v1 = include_bytes!("fixtures/v1.shrunk");
    // v1 header 16, name_len(4), name(7), block_count(4) => block at 31.
    let v1_block = 31usize;
    let mut b = v1.to_vec();
    b[v1_block + 2..v1_block + 6].copy_from_slice(&0u32.to_le_bytes()); // count=0
    check(&b, "zero points");

    let mut b = v1.to_vec();
    b[v1_block] = 99; // unknown codec
    check(&b, "unknown codec");

    let mut b = v1.to_vec();
    b[v1_block + 46..v1_block + 50].copy_from_slice(&u32::MAX.to_le_bytes()); // payload_len
    check(&b, "payload length");

    let _ = fs::remove_dir_all(&dir);
}
