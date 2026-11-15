use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use shrunk::{Agg, Db, Error, Point, Query};

struct Files(PathBuf);

impl Files {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shrunk-lifecycle-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("test.db")
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scan(db: &Db, series: &str) -> Vec<Point> {
    db.query(&Query::new(series, i64::MIN, i64::MAX))
        .unwrap()
        .points
}

fn points(start: i64, end: i64) -> Vec<Point> {
    (start..=end).map(|ts| Point::new(ts, ts as f64)).collect()
}

#[test]
fn deletion_matches_reference_across_blocks_pending_and_empty_ranges() {
    let original = points(-15, 15);
    for block_size in [1, 2, 4, 9, 64] {
        for (start, end) in [
            (-10, 10),
            (-14, -12),
            (11, 14),
            (15, 15),
            (-15, -15),
            (2, 1),
            (50, 60),
            (i64::MIN, i64::MAX),
        ] {
            let mut db = Db::with_block_size(block_size);
            db.write_many("s", &original).unwrap();
            let expected: Vec<_> = original
                .iter()
                .copied()
                .filter(|p| start > end || p.ts < start || p.ts > end)
                .collect();
            let result = db.delete_range("s", start, end).unwrap();
            assert_eq!(
                result.points_deleted as usize,
                original.len() - expected.len()
            );
            assert_eq!(scan(&db, "s"), expected);
            assert_eq!(db.point_count() as usize, expected.len());
            assert_eq!(db.series_names(), vec!["s"]);
            db.flush_all().unwrap();
            assert_eq!(scan(&db, "s"), expected);
            let next = expected.last().map_or(-100, |p| p.ts + 1);
            db.write("s", next, 12.5).unwrap();
            assert_eq!(scan(&db, "s").last().unwrap(), &Point::new(next, 12.5));
        }
    }
}

#[test]
fn deletion_preserves_unaffected_payloads_and_rewrites_only_boundaries() {
    let mut db = Db::with_block_size(4);
    db.write_many("s", &points(0, 19)).unwrap();
    let before = db.summaries()[0].clone();
    let result = db.delete_range("s", 5, 14).unwrap();
    assert_eq!(result.points_deleted, 10);
    assert_eq!(result.blocks_dropped, 1);
    assert_eq!(result.blocks_rewritten, 2);
    assert_eq!(scan(&db, "s"), [points(0, 4), points(15, 19)].concat());
    assert!(db.summaries()[0].payload_bytes < before.payload_bytes);
    assert!(matches!(
        db.delete_series("missing"),
        Err(Error::UnknownSeries(_))
    ));
    assert_eq!(db.delete_series("s").unwrap(), 10);
    assert!(db.series_names().is_empty());
    db.write("s", i64::MIN, 5.0).unwrap();
}

#[test]
fn retained_windows_are_explicit_event_time_and_repeatable() {
    let mut db = Db::with_block_size(3);
    db.write_many("a", &points(-10, 10)).unwrap();
    db.write_many("b", &points(-20, -11)).unwrap();
    assert_eq!(db.retain_since(i64::MIN).unwrap().points_deleted, 0);
    assert_eq!(db.retain_window(5, 10).unwrap().points_deleted, 15);
    assert_eq!(scan(&db, "a"), points(-5, 10));
    assert!(scan(&db, "b").is_empty());
    assert_eq!(db.retain_window(5, 10).unwrap().points_deleted, 0);
    assert_eq!(db.retain_window(i64::MIN, 1).unwrap().points_deleted, 0);
    assert!(matches!(
        db.retain_window(5, 0),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        db.retain_window(5, -1),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(db.retain_since(i64::MAX).unwrap().points_deleted, 16);
    assert_eq!(db.series_names(), vec!["a", "b"]);
}

#[test]
fn downsampling_matches_independent_per_bucket_aggregates() {
    let original: Vec<_> = (-100..=100)
        .map(|ts| Point::new(ts * 3, ((ts * 13) % 17) as f64))
        .collect();
    for block_size in [1, 3, 7, 64, 1024] {
        let mut db = Db::with_block_size(block_size);
        db.write_many("s", &original).unwrap();
        for interval in [1, 7, 30, 1000, i64::MAX] {
            for (start, end) in [(-301, 300), (-77, 98), (200, 100), (1000, 2000)] {
                let mut grouped: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
                for p in original.iter().filter(|p| p.ts >= start && p.ts <= end) {
                    let bucket = (i128::from(p.ts).div_euclid(i128::from(interval))
                        * i128::from(interval))
                    .max(i128::from(i64::MIN)) as i64;
                    grouped.entry(bucket).or_default().push(p.value);
                }
                for agg in [Agg::Count, Agg::Sum, Agg::Avg, Agg::Min, Agg::Max] {
                    let expected: Vec<_> = grouped
                        .iter()
                        .map(|(ts, values)| {
                            let value = match agg {
                                Agg::Count => values.len() as f64,
                                Agg::Sum => values.iter().sum(),
                                Agg::Avg => values.iter().sum::<f64>() / values.len() as f64,
                                Agg::Min => values.iter().copied().fold(f64::INFINITY, f64::min),
                                Agg::Max => {
                                    values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
                                }
                            };
                            Point::new(*ts, value)
                        })
                        .collect();
                    let actual = db.downsample("s", start, end, interval, agg).unwrap();
                    assert_eq!(actual.points, expected);
                    assert_eq!(
                        actual.stats.blocks_total,
                        actual.stats.blocks_skipped_time + actual.stats.blocks_visited
                    );
                }
            }
        }
    }
}

#[test]
fn downsampling_uses_block_metadata_and_handles_timestamp_extremes() {
    let mut db = Db::with_block_size(4);
    db.write_many("s", &points(0, 15)).unwrap();
    let result = db.downsample("s", 0, 15, 8, Agg::Avg).unwrap();
    assert_eq!(result.points, vec![Point::new(0, 3.5), Point::new(8, 11.5)]);
    assert_eq!(result.stats.fast_blocks, 4);
    assert_eq!(result.stats.blocks_decoded, 0);
    db.write_many(
        "extreme",
        &[
            Point::new(i64::MIN, 1.0),
            Point::new(i64::MIN + 1, 3.0),
            Point::new(-1, 7.0),
            Point::new(0, 8.0),
            Point::new(i64::MAX, 9.0),
        ],
    )
    .unwrap();
    let result = db
        .downsample("extreme", i64::MIN, i64::MAX, i64::MAX, Agg::Count)
        .unwrap();
    assert_eq!(
        result.points,
        vec![
            Point::new(i64::MIN, 1.0),
            Point::new(-i64::MAX, 2.0),
            Point::new(0, 1.0),
            Point::new(i64::MAX, 1.0)
        ]
    );
    assert!(matches!(
        db.downsample("s", 0, 15, 0, Agg::Avg),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        db.downsample("s", 0, 15, -10, Agg::Avg),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn downsampling_matches_existing_nan_and_infinity_semantics() {
    let mut db = Db::with_block_size(2);
    db.write_many(
        "s",
        &[
            Point::new(0, f64::NAN),
            Point::new(1, f64::NAN),
            Point::new(10, f64::NAN),
            Point::new(11, 4.0),
            Point::new(20, f64::INFINITY),
            Point::new(21, f64::NEG_INFINITY),
            Point::new(30, -0.0),
            Point::new(31, 0.0),
        ],
    )
    .unwrap();
    for agg in [Agg::Count, Agg::Sum, Agg::Avg, Agg::Min, Agg::Max] {
        let output = db.downsample("s", 0, 39, 10, agg).unwrap();
        let expected: Vec<_> = (0..4)
            .filter_map(|bucket| {
                let ts = bucket * 10;
                db.aggregate("s", ts, ts + 9, agg, None)
                    .unwrap()
                    .value
                    .map(|v| Point::new(ts, v))
            })
            .collect();
        assert_eq!(output.points, expected);
    }
}

#[test]
fn durable_deletion_never_resurrects_wal_points_and_allows_new_appends() {
    let files = Files::new();
    let path = files.db();
    let mut db = Db::open_rw_with_block_size(&path, 4).unwrap();
    db.append("s", &points(0, 5)).unwrap();
    db.checkpoint().unwrap();
    // Exercise old snapshot plus new WAL, including deletion of the tail.
    db.append("s", &points(6, 13)).unwrap();
    let old_reader = Db::open(&path).unwrap();
    assert_eq!(db.delete_range("s", 3, 13).unwrap().points_deleted, 11);
    assert_eq!(scan(&Db::open(&path).unwrap(), "s"), points(0, 2));
    assert_eq!(scan(&old_reader, "s"), points(0, 13));
    db.append("s", &points(3, 8)).unwrap();
    drop(db);
    let mut db = Db::open_rw(&path).unwrap();
    assert_eq!(scan(&db, "s"), points(0, 8));
    db.retain_since(7).unwrap();
    db.append("s", &[Point::new(9, 12.0)]).unwrap();
    drop(db);
    let mut db = Db::open_rw(&path).unwrap();
    assert_eq!(
        scan(&db, "s"),
        vec![Point::new(7, 7.0), Point::new(8, 8.0), Point::new(9, 12.0)]
    );
    db.delete_series("s").unwrap();
    db.append("s", &[Point::new(-100, 2.0)]).unwrap();
    drop(db);
    assert_eq!(
        scan(&Db::open(&path).unwrap(), "s"),
        vec![Point::new(-100, 2.0)]
    );
}

#[test]
fn rollup_is_atomic_durable_and_rejects_existing_targets_on_retry() {
    let files = Files::new();
    let path = files.db();
    let mut db = Db::open_rw_with_block_size(&path, 3).unwrap();
    db.append("raw", &points(-9, 20)).unwrap();
    let expected = db.downsample("raw", -7, 17, 10, Agg::Avg).unwrap().points;
    assert_eq!(db.rollup("raw", "ten_ms", -7, 17, 10, Agg::Avg).unwrap(), 3);
    assert_eq!(scan(&db, "raw"), points(-9, 20));
    assert_eq!(scan(&db, "ten_ms"), expected);
    assert!(matches!(
        db.rollup("raw", "ten_ms", -7, 17, 10, Agg::Avg),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        db.rollup("raw", "raw", -7, 17, 10, Agg::Avg),
        Err(Error::InvalidInput(_))
    ));
    // Empty outputs still reserve the target name for predictable retries.
    assert_eq!(
        db.rollup("raw", "empty", 100, 200, 10, Agg::Count).unwrap(),
        0
    );
    assert!(matches!(
        db.rollup("raw", "empty", 100, 200, 10, Agg::Count),
        Err(Error::InvalidInput(_))
    ));
    db.append("raw", &[Point::new(21, 100.0)]).unwrap();
    drop(db);
    let reopened = Db::open(&path).unwrap();
    assert_eq!(scan(&reopened, "ten_ms"), expected);
    assert!(scan(&reopened, "empty").is_empty());
    assert_eq!(scan(&reopened, "raw").len(), 31);
}

#[test]
fn read_handles_change_only_in_memory_until_saved() {
    let files = Files::new();
    let path = files.db();
    let mut db = Db::new();
    db.write_many("s", &points(0, 5)).unwrap();
    db.save(&path).unwrap();
    let mut read = Db::open(&path).unwrap();
    read.delete_range("s", 2, 5).unwrap();
    assert_eq!(scan(&Db::open(&path).unwrap(), "s"), points(0, 5));
    read.save(&path).unwrap();
    assert_eq!(scan(&Db::open(&path).unwrap(), "s"), points(0, 1));
}

#[test]
fn cli_exposes_buckets_rollups_deletion_and_event_time_retention() {
    use std::process::{Command, Output};

    fn run(path: &std::path::Path, command: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_shrunk"))
            .arg(command)
            .arg(path)
            .args(args)
            .output()
            .unwrap()
    }

    fn successful(output: Output) -> String {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    let files = Files::new();
    let path = files.db();
    successful(run(
        &path,
        "seed",
        &[
            "--series",
            "raw",
            "--pattern",
            "counter",
            "--points",
            "21",
            "--start",
            "-10",
            "--interval",
            "1",
            "--block-size",
            "4",
        ],
    ));
    let buckets = "timestamp,value\n-10,10\n0,10\n10,1\n";
    assert_eq!(
        successful(run(
            &path,
            "query",
            &["--series", "raw", "--agg", "count", "--every", "10", "--csv",]
        )),
        buckets
    );
    successful(run(
        &path,
        "rollup",
        &[
            "--series", "raw", "--target", "counts", "--every", "10", "--agg", "count",
        ],
    ));
    assert_eq!(
        successful(run(&path, "query", &["--series", "counts", "--csv"])),
        buckets
    );
    assert!(!run(
        &path,
        "rollup",
        &["--series", "raw", "--target", "counts", "--every", "10", "--agg", "count",]
    )
    .status
    .success());
    assert!(successful(run(
        &path,
        "delete",
        &["--series", "raw", "--from", "-5", "--to", "4",]
    ))
    .contains("deleted 10 points"));
    assert!(
        successful(run(&path, "retain", &["--now", "10", "--window", "5"]))
            .contains("retained timestamps >= 5")
    );
    assert_eq!(
        successful(run(&path, "query", &["--series", "raw", "--agg", "count"])),
        "6.000000\n"
    );
    assert_eq!(
        successful(run(&path, "query", &["--series", "counts", "--csv"])),
        "timestamp,value\n10,1\n"
    );
    assert!(!run(
        &path,
        "delete",
        &["--series", "raw", "--drop-series", "--from", "0"]
    )
    .status
    .success());
    successful(run(&path, "delete", &["--series", "raw", "--drop-series"]));
    assert!(!run(&path, "query", &["--series", "raw"]).status.success());
    assert!(!run(&path, "retain", &["--since", "5", "--window", "10"])
        .status
        .success());
    assert!(
        !run(&path, "query", &["--series", "counts", "--every", "10"])
            .status
            .success()
    );
    assert!(!run(
        &path,
        "query",
        &["--series", "counts", "--every", "10", "--agg", "count", "--gt", "0",]
    )
    .status
    .success());
}
