//! Optional service policy uses wall-clock time. HTTP retention remains an
//! explicit caller-supplied event-time operation.

use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shrunk::{Db, DeleteResult};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{ApiError, AppState};

#[derive(Clone, Copy)]
pub(super) struct RetentionPolicy {
    window_ms: i64,
    cadence: Duration,
}

impl RetentionPolicy {
    pub(super) fn from_env() -> Result<Option<Self>, String> {
        Self::parse(
            env::var("SHRUNK_RETENTION_MS")
                .ok()
                .filter(|v| !v.is_empty())
                .as_deref(),
            env::var("SHRUNK_RETENTION_INTERVAL_MS")
                .ok()
                .filter(|v| !v.is_empty())
                .as_deref(),
        )
    }

    fn parse(window: Option<&str>, cadence: Option<&str>) -> Result<Option<Self>, String> {
        let Some(window) = window else {
            if cadence.is_some() {
                return Err("SHRUNK_RETENTION_INTERVAL_MS requires SHRUNK_RETENTION_MS".into());
            }
            return Ok(None);
        };
        let window_ms: i64 = window
            .parse()
            .map_err(|_| "SHRUNK_RETENTION_MS must be a positive i64 millisecond duration")?;
        if window_ms <= 0 {
            return Err("SHRUNK_RETENTION_MS must be positive".into());
        }
        let cadence_ms: u64 = cadence
            .unwrap_or("60000")
            .parse()
            .map_err(|_| "SHRUNK_RETENTION_INTERVAL_MS must be in 1..=86400000")?;
        if !(1..=86_400_000).contains(&cadence_ms) {
            return Err("SHRUNK_RETENTION_INTERVAL_MS must be in 1..=86400000".into());
        }
        Ok(Some(Self {
            window_ms,
            cadence: Duration::from_millis(cadence_ms),
        }))
    }

    pub(super) fn sweep(self, db: &mut Db) -> Result<DeleteResult, ApiError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .ok_or_else(|| {
                ApiError::internal("wall clock is outside the supported timestamp range")
            })?;
        self.sweep_at(db, now)
    }

    fn sweep_at(self, db: &mut Db, now: i64) -> Result<DeleteResult, ApiError> {
        let cutoff = now.saturating_sub(self.window_ms);
        // retain_since retries any pending snapshot directory sync before its
        // cheap per-series check for expired points, even on an idle tick.
        db.retain_since(cutoff).map_err(Into::into)
    }

    pub(super) fn report(result: DeleteResult) {
        if result.points_deleted > 0 {
            eprintln!("retention removed {} expired points", result.points_deleted);
        }
    }

    pub(super) fn start(self, state: AppState) -> (watch::Sender<bool>, JoinHandle<()>) {
        let (stop, mut receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut ticks =
                tokio::time::interval_at(tokio::time::Instant::now() + self.cadence, self.cadence);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = receiver.changed() => break,
                    _ = ticks.tick() => {
                        // Wait in the semaphore's FIFO queue instead of
                        // losing every tick while HTTP requests are busy.
                        // Shutdown can cancel this wait; once admitted, the
                        // sweep finishes before the task exits.
                        let permit = tokio::select! {
                            biased;
                            _ = receiver.changed() => break,
                            result = state.acquire_maintenance_permit() => match result {
                                Ok(permit) => permit,
                                Err(error) => {
                                    eprintln!("retention permit failed: {}", error.2);
                                    break;
                                }
                            }
                        };
                        let result = state.run_permitted(permit, move |store| {
                            Self::report(self.sweep(&mut store.db)?);
                            Ok(serde_json::Value::Null)
                        }).await;
                        if let Err(error) = result {
                            eprintln!("retention sweep failed; will retry at next interval: {}", error.2);
                        }
                    }
                }
            }
        });
        (stop, task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use shrunk::{Point, Query};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tokio::sync::Semaphore;

    #[test]
    fn configuration_is_opt_in_and_rejects_invalid_durations() {
        assert!(RetentionPolicy::parse(None, None).unwrap().is_none());
        for (window, cadence) in [
            (None, Some("100")),
            (Some("0"), None),
            (Some("-1"), None),
            (Some("broken"), None),
            (Some("1000"), Some("0")),
            (Some("1000"), Some("86400001")),
        ] {
            assert!(RetentionPolicy::parse(window, cadence).is_err());
        }
        let policy = RetentionPolicy::parse(Some("1000"), None).unwrap().unwrap();
        assert_eq!(policy.cadence, Duration::from_secs(60));
    }

    #[test]
    fn policy_keeps_the_cutoff_and_future_points_and_is_repeatable() {
        let policy = RetentionPolicy::parse(Some("10"), Some("100"))
            .unwrap()
            .unwrap();
        let mut db = Db::with_block_size(2);
        db.write_many(
            "s",
            &[
                Point::new(89, 1.0),
                Point::new(90, 2.0),
                Point::new(110, 3.0),
            ],
        )
        .unwrap();
        assert_eq!(
            policy.sweep_at(&mut db, 100).ok().unwrap().points_deleted,
            1
        );
        assert_eq!(
            policy.sweep_at(&mut db, 100).ok().unwrap().points_deleted,
            0
        );
        assert_eq!(
            db.query(&Query::new("s", i64::MIN, i64::MAX))
                .unwrap()
                .points,
            vec![Point::new(90, 2.0), Point::new(110, 3.0)]
        );
        assert_eq!(
            policy
                .sweep_at(&mut db, i64::MIN)
                .ok()
                .unwrap()
                .points_deleted,
            0
        );
    }

    #[tokio::test]
    async fn maintenance_waits_for_an_operation_permit() {
        let state = AppState {
            store: Arc::new(Mutex::new(Store {
                db: Db::new(),
                wal_path: PathBuf::new(),
            })),
            token: Arc::from(&b"test-token"[..]),
            requests: Arc::new(Semaphore::new(1)),
            operations: Arc::new(Semaphore::new(1)),
        };
        let held = state.operations.clone().acquire_owned().await.unwrap();
        let waiter = tokio::spawn({
            let state = state.clone();
            async move {
                state
                    .acquire_maintenance_permit()
                    .await
                    .map_err(|error| error.2)
                    .unwrap()
            }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(held);
        let acquired = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.operations.available_permits(), 0);
        drop(acquired);
        assert_eq!(state.operations.available_permits(), 1);
    }
}
