use std::fs;

use shrunk::{Db, Point, Query};

fn fixture(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir =
        std::env::temp_dir().join(format!("shrunk-adversarial-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("db.shrunk");
    let mut wal = db.as_os_str().to_os_string();
    wal.push(".wal");
    (db, wal.into())
}

#[test]
fn rw_open_repairs_torn_suffix_before_appending() {
    let (path, wal_path) = fixture("torn-tail");
    let mut db = Db::open_rw_with_block_size(&path, 64).unwrap();
    db.append("cpu", &[Point::new(1, 10.0)]).unwrap();
    drop(db);

    let mut wal = fs::OpenOptions::new().append(true).open(&wal_path).unwrap();
    use std::io::Write;
    wal.write_all(&[0x55, 0x66, 0x77]).unwrap();
    drop(wal);

    let mut db = Db::open_rw(&path).unwrap();
    db.append("cpu", &[Point::new(2, 20.0)]).unwrap();
    drop(db);

    let recovered = Db::open(&path).unwrap();
    let result = recovered.query(&Query::new("cpu", 0, 10)).unwrap();
    assert_eq!(
        result.points,
        vec![Point::new(1, 10.0), Point::new(2, 20.0)]
    );
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn replay_rejects_corruption_before_a_later_record() {
    let (path, wal_path) = fixture("mid-corruption");
    let mut db = Db::open_rw_with_block_size(&path, 64).unwrap();
    db.append("cpu", &[Point::new(1, 10.0)]).unwrap();
    db.append("cpu", &[Point::new(2, 20.0)]).unwrap();
    drop(db);

    let mut bytes = fs::read(&wal_path).unwrap();
    let first_payload_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    let first_record_end = 16 + 4 + first_payload_len + 4;
    bytes[first_record_end - 1] ^= 1;
    fs::write(&wal_path, bytes).unwrap();

    let error = Db::open(&path).unwrap_err();
    assert!(error.to_string().contains("checksum mismatch"));
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
