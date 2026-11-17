use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::admission::{Config, Executor, Worker};
use crate::Store;

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct TestStore(pub PathBuf);

impl TestStore {
    pub fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "shrunk-service-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        Self(directory)
    }

    pub fn store(&self) -> Store {
        Store {
            db: shrunk::Db::open_rw(&self.0.join("shrunk.db")).unwrap(),
            wal_path: self.0.join("shrunk.db.wal"),
        }
    }

    pub fn executor(&self, maximum: usize) -> (Executor, Worker) {
        Executor::start(
            self.store(),
            Config {
                max_inflight: maximum,
                queue_timeout: Duration::from_millis(100),
                max_connections: 16,
            },
        )
        .unwrap()
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
