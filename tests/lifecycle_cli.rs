use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct Files(PathBuf);

impl Files {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shrunk-lifecycle-cli-{}-{}",
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
