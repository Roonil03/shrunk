use shrunk::{Db, Point, Query};
use std::fs;

fn wal_path(db: &std::path::Path) -> std::path::PathBuf {
    let mut p = db.as_os_str().to_os_string();
    p.push(".wal");
    p.into()
}

#[test]
fn appending_after_torn_tail_makes_later_acknowledged_records_unrecoverable() {
    let dir = std::env::temp_dir().join(format!("shrunk-adversarial-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.shrunk");
    let wal = wal_path(&path);

    let mut db = Db::open_rw_with_block_size(&path, 64).unwrap();
    db.append("m", &[Point::new(1, 10.0)]).unwrap();
    db.append("m", &[Point::new(2, 20.0)]).unwrap();
    drop(db);

    // Simulate a crash that leaves a short second record. The first batch is
    // durable; the second batch is uncommitted and may be discarded.
    let bytes = fs::read(&wal).unwrap();
    fs::write(&wal, &bytes[..bytes.len() - 3]).unwrap();

    // Recovery tolerates the torn tail. This subsequent append returns Ok and
    // is therefore acknowledged, but is physically placed behind the tail.
    let mut resumed = Db::open_rw(&path).unwrap();
    resumed.append("m", &[Point::new(2, 200.0)]).unwrap();
    drop(resumed);

    let recovered = Db::open(&path).unwrap();
    let got = recovered
        .query(&Query::new("m", i64::MIN, i64::MAX))
        .unwrap()
        .points;
    assert_eq!(got, vec![Point::new(1, 10.0), Point::new(2, 200.0)]);
}

#[test]
fn read_only_open_ignores_but_preserves_a_torn_wal_tail() {
    let dir = std::env::temp_dir().join(format!("shrunk-readonly-wal-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.shrunk");
    let wal = wal_path(&path);

    let mut db = Db::open_rw_with_block_size(&path, 64).unwrap();
    db.append("m", &[Point::new(1, 10.0)]).unwrap();
    db.append("m", &[Point::new(2, 20.0)]).unwrap();
    drop(db);

    let bytes = fs::read(&wal).unwrap();
    fs::write(&wal, &bytes[..bytes.len() - 3]).unwrap();
    let torn_bytes = fs::read(&wal).unwrap();

    let recovered = Db::open(&path).unwrap();
    let got = recovered
        .query(&Query::new("m", i64::MIN, i64::MAX))
        .unwrap()
        .points;
    assert_eq!(got, vec![Point::new(1, 10.0)]);
    assert_eq!(fs::read(&wal).unwrap(), torn_bytes);
}
