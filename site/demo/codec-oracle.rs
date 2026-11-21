//! Development-only oracle using the actual Rust implementation; never deployed.
//! From the repository root:
//! rustc --edition=2021 --crate-type=rlib --crate-name shrunk src/lib.rs -o /tmp/libshrunk-demo.rlib
//! rustc --edition=2021 site/demo/codec-oracle.rs --extern shrunk=/tmp/libshrunk-demo.rlib -o /tmp/shrunk-codec-oracle
//! /tmp/shrunk-codec-oracle > site/demo/codec-fixtures.json
//! SHRUNK_RUST_ORACLE=/tmp/shrunk-codec-oracle node --test site/demo/codec.test.mjs

use shrunk::{Codec, Db, Point, Query, FORMAT_MAGIC, FORMAT_VERSION};
use std::{env, fs, path::Path};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fixtures() -> Vec<(&'static str, Vec<Point>)> {
    let mut cases = vec![
        ("single-raw-tie", vec![Point::new(-123, -0.0)]),
        ("two-values", vec![Point::new(-1000, 1.5), Point::new(0, -10.75)]),
        ("constant", (0..128).map(|i| Point::new(1_700_000_000_000 + i * 1000, 42.5)).collect()),
        ("smooth", (0..128).map(|i| Point::new(i * 1000, 20.0 + (i as f64 / 8.0).sin())).collect()),
        ("float-extremes", [-f64::MAX, -f64::MIN_POSITIVE, -0.0, 0.0, f64::from_bits(1), f64::MIN_POSITIVE, f64::MAX]
            .into_iter().enumerate().map(|(i, value)| Point::new(i as i64, value)).collect()),
        ("sum-overflow", (0..64).map(|i| Point::new(i, f64::MAX)).collect()),
        ("safe-timestamp-extremes", vec![Point::new(-9_007_199_254_740_991, 1.0), Point::new(0, 2.0), Point::new(9_007_199_254_740_991, 3.0)]),
    ];
    let mut ts = -1_000_000;
    let mut delta = 10_000;
    let mut boundary = vec![Point::new(ts, 1.0)];
    for dod in [0, 0, -64, 63, -65, 64, -256, 255, -257, 256, -2048, 2047, -2049, 2048, 0] {
        delta += dod;
        ts += delta;
        boundary.push(Point::new(ts, 1.0));
    }
    cases.push(("timestamp-prefix-boundaries", boundary));
    // Exercise narrow/broad window transitions and the metadata-cost rule.
    cases.push(("xor-windows", [0x3ff0000000000000, 0x3ff0000000000001, 0x3ff0000010000001,
        0x3ff0000010000000, 0xbff0000010000000, 0xbff0000000000000, 0xbff0000000000001,
        0x3ff8000000000001, 0x3ff8000000000000, 0x3ff8000000000000]
        .into_iter().enumerate().map(|(i, bits)| Point::new(i as i64 * 1000, f64::from_bits(bits))).collect()));
    let mut state = 0x12345678abcdef01u64;
    let random = (0..128).map(|i| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        Point::new(i * 1000, f64::from_bits(state & 0xffef_ffff_ffff_ffff))
    }).collect();
    cases.push(("random-raw-fallback", random));
    cases
}

fn verify(raw_path: &str, block_path: &str) {
    let raw = fs::read(raw_path).unwrap();
    let points = Codec::Raw.decode(&raw, raw.len() / 16).unwrap();
    let block = fs::read(block_path).unwrap();
    let gorilla = Codec::Gorilla.encode(&points).unwrap();
    let (codec, expected) = if gorilla.len() < raw.len() { (Codec::Gorilla, gorilla) } else { (Codec::Raw, raw) };
    assert_eq!(block[0], codec as u8);
    assert_eq!(&block[54..block.len() - 4], expected);
    assert_eq!(codec.decode(&expected, points.len()).unwrap(), points);

    // Wrap the emitted block in a real v3 snapshot. Db::open verifies the
    // header CRC/metadata; query verifies the payload CRC and decodes it.
    let mut snapshot = Vec::new();
    snapshot.extend(FORMAT_MAGIC);
    snapshot.extend(FORMAT_VERSION.to_le_bytes());
    snapshot.extend(0u16.to_le_bytes());
    snapshot.extend((points.len() as u32).to_le_bytes());
    snapshot.extend(1u32.to_le_bytes()); // one series
    snapshot.extend(4u32.to_le_bytes());
    snapshot.extend(b"demo");
    snapshot.extend(1u32.to_le_bytes()); // one block
    snapshot.extend(block);
    let snapshot_path = format!("{block_path}.shrunk");
    fs::write(&snapshot_path, snapshot).unwrap();
    let db = Db::open(Path::new(&snapshot_path)).unwrap();
    let result = db.query(&Query::new("demo", i64::MIN, i64::MAX)).unwrap();
    assert_eq!(result.points, points);
    fs::remove_file(snapshot_path).unwrap();
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() == 3 {
        verify(&args[1], &args[2]);
        return;
    }
    assert_eq!(args.len(), 1);
    println!("[");
    let fixtures = fixtures();
    for (index, (name, points)) in fixtures.iter().enumerate() {
        println!("  {{\"name\":\"{name}\",\"raw\":\"{}\",\"gorilla\":\"{}\"}}{}",
            hex(&Codec::Raw.encode(points).unwrap()), hex(&Codec::Gorilla.encode(points).unwrap()),
            if index + 1 == fixtures.len() { "" } else { "," });
    }
    println!("]");
}
