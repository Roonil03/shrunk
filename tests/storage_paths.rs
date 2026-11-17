use std::fs;
use std::path::Path;
use std::process::Command;

use shrunk::{Agg, Db, Point, Query};

// Each cwd change runs in a child process. The parent has no database file
// descriptors open when spawning, so it cannot inherit another writer's lock.
#[test]
fn relative_writer_paths_survive_cwd_changes() {
    for case in ["wal-only", "snapshot"] {
        run_case(case);
    }
}

#[cfg(unix)]
#[test]
fn saving_through_a_parent_symlink_keeps_the_original_binding() {
    run_case("parent-alias");
}

fn run_case(case: &str) {
    let dir = std::env::temp_dir().join(format!(
        "shrunk-storage-paths-{case}-{}",
        std::process::id()
    ));
    fs::create_dir(&dir).unwrap();
    fs::create_dir(dir.join("original")).unwrap();
    fs::create_dir(dir.join("elsewhere")).unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "storage_paths_child", "--nocapture"])
        .env("SHRUNK_STORAGE_PATH_CASE", case)
        .current_dir(dir.join("original"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "case {case}:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn storage_paths_child() {
    let Ok(case) = std::env::var("SHRUNK_STORAGE_PATH_CASE") else {
        return;
    };
    let original = std::env::current_dir().unwrap();
    let elsewhere = original.parent().unwrap().join("elsewhere");
    let original_path = original.join("metrics.db");
    let original_wal = original.join("metrics.db.wal");
    let decoy_path = elsewhere.join("metrics.db");
    let decoy_wal = elsewhere.join("metrics.db.wal");

    let mut decoy = Db::open_rw_with_block_size(&decoy_path, 4).unwrap();
    decoy.write("decoy", 90, 90.0).unwrap();
    decoy.checkpoint().unwrap();
    decoy.write("decoy", 100, 100.0).unwrap();
    drop(decoy);
    let decoy_snapshot_before = fs::read(&decoy_path).unwrap();
    let decoy_wal_before = fs::read(&decoy_wal).unwrap();

    let input = Path::new("./metrics.db").to_path_buf();
    #[cfg(unix)]
    let input = if case == "parent-alias" {
        let alias = original.parent().unwrap().join("alias");
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        alias.join("metrics.db")
    } else {
        input
    };
    if case != "wal-only" {
        let mut snapshot = Db::with_block_size(4);
        snapshot.write("s", 1, 1.0).unwrap();
        snapshot.save(&input).unwrap();
    }
    let mut db = Db::open_rw_with_block_size(&input, 4).unwrap();
    if case == "wal-only" {
        db.write("s", 1, 1.0).unwrap();
        assert!(!original_path.exists());
    }
    db.write("s", 2, 2.0).unwrap();

    #[cfg(unix)]
    if case == "parent-alias" {
        // A successful save must not replace the bound canonical path with
        // the caller's symlink-containing spelling.
        db.save(&input).unwrap();
        let alias = original.parent().unwrap().join("alias");
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &alias).unwrap();
        assert!(db.save(&input).is_err());
    }

    std::env::set_current_dir(&elsewhere).unwrap();
    // The same relative spelling now names the decoy, and must be rejected.
    assert!(db.save(Path::new("./metrics.db")).is_err());
    if case == "wal-only" {
        assert!(!original_path.exists());
    }
    db.write("s", 3, 3.0).unwrap();
    db.checkpoint().unwrap();
    assert_eq!(fs::metadata(&original_wal).unwrap().len(), 0);
    assert_eq!(Db::open(&original_path).unwrap().point_count(), 3);

    // Lifecycle publication and subsequent appends use the same binding.
    db.write("s", 4, 4.0).unwrap();
    assert_eq!(db.delete_range("s", 1, 2).unwrap().points_deleted, 2);
    assert_eq!(db.retain_since(4).unwrap().points_deleted, 1);
    assert_eq!(db.rollup("s", "rollup", 4, 4, 1, Agg::Sum).unwrap(), 1);
    assert_eq!(db.delete_series("rollup").unwrap(), 1);
    db.write("s", 5, 5.0).unwrap();
    drop(db);

    let expected = [Point::new(4, 4.0), Point::new(5, 5.0)];
    for recovered in [
        Db::open(&original_path).unwrap(),
        Db::open_rw(&original_path).unwrap(),
    ] {
        assert_eq!(
            recovered.query(&Query::new("s", 0, 10)).unwrap().points,
            expected
        );
    }
    assert_eq!(fs::read(&decoy_path).unwrap(), decoy_snapshot_before);
    assert_eq!(fs::read(&decoy_wal).unwrap(), decoy_wal_before);
    assert_eq!(Db::open(&decoy_path).unwrap().point_count(), 2);
    std::env::set_current_dir(original.parent().unwrap()).unwrap();
}
