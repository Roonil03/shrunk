//! Private single-owner boundary: HTTP and policy producers submit commands,
//! and only this FIFO worker may touch Store. No storage/routing API is exposed.

use std::collections::VecDeque;
use std::env;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{oneshot, Notify};

use crate::{ApiError, Store};

const PENDING: u8 = 0;
const RUNNING: u8 = 1;
const CANCELLED: u8 = 2;

type Operation = Box<dyn FnOnce(&mut Store) -> Result<Value, ApiError> + Send>;

#[derive(Clone, Copy)]
pub(super) struct Config {
    pub max_inflight: usize,
    pub queue_timeout: Duration,
    pub max_connections: usize,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let max_inflight = positive("SHRUNK_MAX_INFLIGHT_OPERATIONS", 8)?;
        let max_connections = positive("SHRUNK_MAX_CONNECTIONS", 16)?;
        // Tokio semaphores reserve high bits for bookkeeping.
        if max_connections > tokio::sync::Semaphore::MAX_PERMITS {
            return Err("SHRUNK_MAX_CONNECTIONS is too large".into());
        }
        let queue_ms = positive("SHRUNK_QUEUE_TIMEOUT_MS", 1000)?;
        let queue_timeout = Duration::from_millis(
            u64::try_from(queue_ms).map_err(|_| "SHRUNK_QUEUE_TIMEOUT_MS is too large")?,
        );
        if Instant::now().checked_add(queue_timeout).is_none() {
            return Err("SHRUNK_QUEUE_TIMEOUT_MS is too large".into());
        }
        Ok(Self {
            max_inflight,
            queue_timeout,
            max_connections,
        })
    }
}

fn positive(name: &str, default: usize) -> Result<usize, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("{name} must be a positive integer")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("{name}: {error}")),
    }
}

#[derive(Default)]
pub(super) struct Metrics {
    pub active_connections: AtomicUsize,
    pub connection_rejections: AtomicU64,
    pub http_rejections: AtomicU64,
    queued: AtomicUsize,
    running: AtomicUsize,
    target: AtomicUsize,
    admission_rejections: AtomicU64,
    queue_expiries: AtomicU64,
    pending_cancellations: AtomicU64,
    completed: AtomicU64,
    last_queue_us: AtomicU64,
    last_operation_us: AtomicU64,
    ewma_operation_us: AtomicU64,
}

impl Metrics {
    pub fn snapshot(&self, config: Config) -> Value {
        // Independent, approximate counters: status never waits for the Store
        // or even the admission mutex, including during a long checkpoint.
        json!({
            "active_connections": self.active_connections.load(Ordering::Relaxed),
            "max_connections": config.max_connections,
            "queued_operations": self.queued.load(Ordering::Relaxed),
            "running_operations": self.running.load(Ordering::Relaxed),
            "admission_target": self.target.load(Ordering::Relaxed),
            "max_inflight_operations": config.max_inflight,
            "queue_timeout_ms": config.queue_timeout.as_millis(),
            "connection_rejections": self.connection_rejections.load(Ordering::Relaxed),
            "http_rejections": self.http_rejections.load(Ordering::Relaxed),
            "admission_rejections": self.admission_rejections.load(Ordering::Relaxed),
            "queue_expiries": self.queue_expiries.load(Ordering::Relaxed),
            "pending_cancellations": self.pending_cancellations.load(Ordering::Relaxed),
            "completed_operations": self.completed.load(Ordering::Relaxed),
            "last_queue_us": self.last_queue_us.load(Ordering::Relaxed),
            "last_operation_us": self.last_operation_us.load(Ordering::Relaxed),
            "ewma_operation_us": self.ewma_operation_us.load(Ordering::Relaxed),
        })
    }
}

struct Command {
    operation: Operation,
    reply: oneshot::Sender<Result<Value, ApiError>>,
    state: Arc<AtomicU8>,
    submitted: Instant,
    deadline: Option<Instant>,
}

struct Gate {
    queue: VecDeque<Command>,
    outstanding: usize,
    target: usize,
    maintenance_waiting: bool,
    accepting: bool,
}

struct Shared {
    gate: Mutex<Gate>,
    ready: Condvar,
    capacity: Notify,
    metrics: Arc<Metrics>,
    config: Config,
}

#[derive(Clone)]
pub(super) struct Executor(Arc<Shared>);

pub(super) struct Worker {
    executor: Executor,
    thread: Option<thread::JoinHandle<Result<(), ApiError>>>,
}

impl Worker {
    pub async fn finish(mut self) -> Result<(), ApiError> {
        self.executor.close();
        let thread = self.thread.take().expect("worker joined once");
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .map_err(|_| ApiError::internal("database worker join failed"))?
            .map_err(|_| ApiError::internal("database worker failed"))?
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.executor.close();
    }
}

impl Executor {
    pub fn start(mut store: Store, config: Config) -> std::io::Result<(Self, Worker)> {
        let metrics = Arc::new(Metrics::default());
        metrics.target.store(config.max_inflight, Ordering::Relaxed);
        let executor = Self(Arc::new(Shared {
            gate: Mutex::new(Gate {
                queue: VecDeque::new(),
                outstanding: 0,
                target: config.max_inflight,
                maintenance_waiting: false,
                accepting: true,
            }),
            ready: Condvar::new(),
            capacity: Notify::new(),
            metrics,
            config,
        }));
        let shared = executor.0.clone();
        let thread = thread::Builder::new()
            .name("shrunk-store".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    work(&shared, &mut store)
                }));
                match result {
                    Ok(result) => result,
                    Err(_) => {
                        let mut gate = shared.gate.lock().expect("admission mutex");
                        gate.accepting = false;
                        gate.queue.clear();
                        gate.outstanding = 0;
                        shared.metrics.queued.store(0, Ordering::Relaxed);
                        shared.metrics.running.store(0, Ordering::Relaxed);
                        drop(gate);
                        shared.capacity.notify_waiters();
                        Err(ApiError::internal("database worker failed"))
                    }
                }
            })?;
        let worker = Worker {
            executor: executor.clone(),
            thread: Some(thread),
        };
        Ok((executor, worker))
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        self.0.metrics.clone()
    }

    pub fn status(&self) -> Value {
        self.0.metrics.snapshot(self.0.config)
    }

    pub fn close(&self) {
        self.0.gate.lock().expect("admission mutex").accepting = false;
        self.0.ready.notify_one();
        self.0.capacity.notify_waiters();
    }

    pub async fn run<F>(&self, operation: F) -> Result<Value, ApiError>
    where
        F: FnOnce(&mut Store) -> Result<Value, ApiError> + Send + 'static,
    {
        let submitted = Instant::now();
        let deadline = submitted + self.0.config.queue_timeout;
        let (command, receiver) = command(operation, submitted, Some(deadline));
        let cancellation = Cancellation(command.state.clone());
        {
            let mut gate = self.0.gate.lock().expect("admission mutex");
            if !gate.accepting || gate.outstanding >= gate.target || gate.maintenance_waiting {
                self.0
                    .metrics
                    .admission_rejections
                    .fetch_add(1, Ordering::Relaxed);
                return Err(ApiError::busy());
            }
            self.push(&mut gate, command);
        }
        let mut receiver = receiver;
        tokio::select! {
            biased;
            result = &mut receiver => result.map_err(|_| ApiError::internal("database worker failed"))?,
            _ = tokio::time::sleep_until(deadline.into()) => {
                if cancellation.0.compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    self.0.metrics.queue_expiries.fetch_add(1, Ordering::Relaxed);
                    // Keep the physical queue entry and its slot until the
                    // worker skips it. A long operation cannot grow a backlog
                    // of cancelled payloads through repeated retries.
                    Err(ApiError::busy())
                } else {
                    receiver.await.map_err(|_| ApiError::internal("database worker failed"))?
                }
            }
        }
    }

    // The policy has only one producer. While it waits, new HTTP work cannot
    // overtake it. Once accepted it shares the same FIFO and is never expired.
    pub async fn maintenance<F>(&self, operation: F) -> Result<Value, ApiError>
    where
        F: FnOnce(&mut Store) -> Result<Value, ApiError> + Send + 'static,
    {
        let waiting = MaintenanceWaiter::new(self.0.clone());
        let mut operation = Some(operation);
        let receiver = loop {
            let notified = self.0.capacity.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut gate = self.0.gate.lock().expect("admission mutex");
                if !gate.accepting {
                    return Err(ApiError::busy());
                }
                if gate.outstanding < gate.target {
                    let (command, receiver) = command(
                        operation.take().expect("one policy command"),
                        Instant::now(),
                        None,
                    );
                    self.push(&mut gate, command);
                    break receiver;
                }
            }
            notified.await;
        };
        drop(waiting);
        // Accepted maintenance is not cancelled when its producer is dropped.
        receiver
            .await
            .map_err(|_| ApiError::internal("database worker failed"))?
    }

    fn push(&self, gate: &mut Gate, command: Command) {
        gate.outstanding += 1;
        gate.queue.push_back(command);
        self.0.metrics.queued.fetch_add(1, Ordering::Relaxed);
        self.0.ready.notify_one();
    }
}

fn command<F>(
    operation: F,
    submitted: Instant,
    deadline: Option<Instant>,
) -> (Command, oneshot::Receiver<Result<Value, ApiError>>)
where
    F: FnOnce(&mut Store) -> Result<Value, ApiError> + Send + 'static,
{
    let (reply, receiver) = oneshot::channel();
    (
        Command {
            operation: Box::new(operation),
            reply,
            state: Arc::new(AtomicU8::new(PENDING)),
            submitted,
            deadline,
        },
        receiver,
    )
}

struct Cancellation(Arc<AtomicU8>);

impl Drop for Cancellation {
    fn drop(&mut self) {
        // Dropping an HTTP future may only cancel work that never started.
        let _ = self
            .0
            .compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire);
    }
}

struct MaintenanceWaiter(Arc<Shared>);

impl MaintenanceWaiter {
    fn new(shared: Arc<Shared>) -> Self {
        shared
            .gate
            .lock()
            .expect("admission mutex")
            .maintenance_waiting = true;
        Self(shared)
    }
}

impl Drop for MaintenanceWaiter {
    fn drop(&mut self) {
        self.0
            .gate
            .lock()
            .expect("admission mutex")
            .maintenance_waiting = false;
        self.0.capacity.notify_waiters();
    }
}

struct AdaptiveTarget {
    max: usize,
    target: usize,
    queue_ns: u128,
    ewma_ns: Option<u128>,
    last_update: Instant,
}

impl AdaptiveTarget {
    fn new(config: Config, now: Instant) -> Self {
        Self {
            max: config.max_inflight,
            target: config.max_inflight,
            queue_ns: config.queue_timeout.as_nanos(),
            ewma_ns: None,
            last_update: now,
        }
    }

    fn observe(&mut self, duration: Duration, now: Instant) -> usize {
        let duration = duration.as_nanos();
        let ewma = match self.ewma_ns {
            Some(previous) => (previous * 7 + duration) / 8,
            None => duration,
        };
        self.ewma_ns = Some(ewma);
        if now.duration_since(self.last_update) >= Duration::from_secs(1) {
            let desired = (1 + self.queue_ns / ewma.max(1_000_000)).min(self.max as u128) as usize;
            self.target = if desired < self.target {
                desired
            } else {
                desired.min(self.target.saturating_add(1))
            };
            self.last_update = now;
        }
        self.target
    }
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn work(shared: &Shared, store: &mut Store) -> Result<(), ApiError> {
    let mut adaptive = AdaptiveTarget::new(shared.config, Instant::now());
    loop {
        let command = {
            let mut gate = shared.gate.lock().expect("admission mutex");
            while gate.queue.is_empty() && gate.accepting {
                gate = shared.ready.wait(gate).expect("admission mutex");
            }
            let Some(command) = gate.queue.pop_front() else {
                break;
            };
            shared.metrics.queued.fetch_sub(1, Ordering::Relaxed);
            command
        };
        let now = Instant::now();
        let expired = command.deadline.is_some_and(|deadline| now >= deadline)
            && command
                .state
                .compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
        let target = if expired {
            shared
                .metrics
                .queue_expiries
                .fetch_add(1, Ordering::Relaxed);
            let _ = command.reply.send(Err(ApiError::busy()));
            adaptive.target
        } else if command
            .state
            .compare_exchange(PENDING, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            shared.metrics.running.store(1, Ordering::Relaxed);
            shared.metrics.last_queue_us.store(
                micros(now.duration_since(command.submitted)),
                Ordering::Relaxed,
            );
            let result = (command.operation)(store);
            let finished = Instant::now();
            let duration = finished.duration_since(now);
            let target = adaptive.observe(duration, finished);
            shared
                .metrics
                .last_operation_us
                .store(micros(duration), Ordering::Relaxed);
            shared.metrics.ewma_operation_us.store(
                (adaptive.ewma_ns.unwrap_or(0) / 1000).min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
            shared.metrics.completed.fetch_add(1, Ordering::Relaxed);
            shared.metrics.running.store(0, Ordering::Relaxed);
            let _ = command.reply.send(result);
            target
        } else {
            shared
                .metrics
                .pending_cancellations
                .fetch_add(1, Ordering::Relaxed);
            adaptive.target
        };
        {
            let mut gate = shared.gate.lock().expect("admission mutex");
            gate.outstanding -= 1;
            gate.target = target;
            shared.metrics.target.store(target, Ordering::Relaxed);
        }
        shared.capacity.notify_waiters();
    }
    // Admission is closed, policy producers have been stopped, and every
    // accepted command (even one whose HTTP client disappeared) is drained.
    store.db.checkpoint()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestStore;
    use shrunk::{Point, Query};
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    async fn blocked(
        executor: &Executor,
    ) -> (
        mpsc::Sender<()>,
        tokio::task::JoinHandle<Result<Value, ApiError>>,
    ) {
        let (release, wait) = mpsc::channel();
        let (started, running) = oneshot::channel();
        let executor = executor.clone();
        let task = tokio::spawn(async move {
            executor
                .run(move |_| {
                    let _ = started.send(());
                    wait.recv().unwrap();
                    Ok(json!("finished"))
                })
                .await
        });
        running.await.unwrap();
        (release, task)
    }

    async fn queued(executor: &Executor, count: usize) {
        // Only Tokio producers update this condition while the worker is held
        // at an explicit channel barrier; no timing sleeps are needed.
        for _ in 0..10_000 {
            if executor.0.gate.lock().unwrap().queue.len() == count {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("commands were not queued");
    }

    #[tokio::test]
    async fn fifo_includes_running_operation_in_full_queue() {
        let database = TestStore::new();
        let (executor, worker) = database.executor(3);
        let (release, first) = blocked(&executor).await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for n in 1..=2 {
            let client = executor.clone();
            let order = order.clone();
            tasks.push(tokio::spawn(async move {
                client
                    .run(move |_| {
                        order.lock().unwrap().push(n);
                        Ok(json!(n))
                    })
                    .await
            }));
            queued(&executor, n).await;
        }
        assert_eq!(
            executor
                .run(|_| panic!("full command ran"))
                .await
                .unwrap_err()
                .0,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(executor.0.gate.lock().unwrap().outstanding, 3);
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
        assert_eq!(executor.status()["admission_rejections"], 1);
        worker.finish().await.unwrap();
    }

    #[tokio::test]
    async fn expiry_never_runs_and_retains_its_physical_queue_slot() {
        let database = TestStore::new();
        let (executor, worker) = database.executor(2);
        let (release, first) = blocked(&executor).await;
        tokio::time::pause();
        let executed = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn({
            let executor = executor.clone();
            let executed = executed.clone();
            async move {
                executor
                    .run(move |_| {
                        executed.store(true, Ordering::Relaxed);
                        Ok(Value::Null)
                    })
                    .await
            }
        });
        queued(&executor, 1).await;
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(
            task.await.unwrap().unwrap_err().0,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(executor.0.gate.lock().unwrap().outstanding, 2);
        assert_eq!(
            executor
                .run(|_| panic!("expired slot reused too soon"))
                .await
                .unwrap_err()
                .0,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        worker.finish().await.unwrap();
        assert!(!executed.load(Ordering::Relaxed));
        assert_eq!(executor.status()["queue_expiries"], 1);
        assert_eq!(executor.status()["queued_operations"], 0);
    }

    #[tokio::test]
    async fn running_operation_returns_actual_result_after_queue_deadline() {
        let database = TestStore::new();
        let (executor, worker) = database.executor(1);
        let (release, task) = blocked(&executor).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        assert_eq!(executor.status()["queue_expiries"], 0);
        release.send(()).unwrap();
        assert_eq!(task.await.unwrap().unwrap(), json!("finished"));
        worker.finish().await.unwrap();
    }

    #[tokio::test]
    async fn dropped_pending_is_skipped_but_running_work_drains_before_checkpoint() {
        let database = TestStore::new();
        let (executor, worker) = database.executor(2);
        let (release, wait) = mpsc::channel();
        let (started, running) = oneshot::channel();
        let mutation = tokio::spawn({
            let executor = executor.clone();
            async move {
                executor
                    .run(move |store| {
                        let _ = started.send(());
                        wait.recv().unwrap();
                        store.db.append("durable", &[Point::new(1, 5.0)])?;
                        Ok(json!("committed"))
                    })
                    .await
            }
        });
        running.await.unwrap();
        let pending = tokio::spawn({
            let executor = executor.clone();
            async move {
                executor
                    .run(|_| panic!("disconnected pending command ran"))
                    .await
            }
        });
        queued(&executor, 1).await;
        pending.abort();
        let _ = pending.await;
        mutation.abort();
        let _ = mutation.await;
        assert_eq!(executor.0.gate.lock().unwrap().outstanding, 2);
        executor.close();
        assert!(executor
            .run(|_| panic!("admitted after shutdown"))
            .await
            .is_err());
        let drained = tokio::spawn(worker.finish());
        tokio::task::yield_now().await;
        assert!(!drained.is_finished());
        release.send(()).unwrap();
        drained.await.unwrap().unwrap();
        assert_eq!(
            std::fs::metadata(database.0.join("shrunk.db.wal"))
                .unwrap()
                .len(),
            0
        );
        let db = shrunk::Db::open_rw(&database.0.join("shrunk.db")).unwrap();
        assert_eq!(
            db.query(&Query::new("durable", i64::MIN, i64::MAX))
                .unwrap()
                .points,
            vec![Point::new(1, 5.0)]
        );
    }

    #[tokio::test]
    async fn waiting_maintenance_cannot_be_overtaken_and_joins_fifo() {
        let database = TestStore::new();
        let (executor, worker) = database.executor(1);
        let (release, first) = blocked(&executor).await;
        let (swept, sweep) = oneshot::channel();
        let maintenance = tokio::spawn({
            let executor = executor.clone();
            async move {
                executor
                    .maintenance(move |_| {
                        let _ = swept.send(());
                        Ok(Value::Null)
                    })
                    .await
            }
        });
        for _ in 0..10_000 {
            if executor.0.gate.lock().unwrap().maintenance_waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(executor.0.gate.lock().unwrap().maintenance_waiting);
        assert!(executor
            .run(|_| panic!("overtook maintenance"))
            .await
            .is_err());
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        sweep.await.unwrap();
        maintenance.await.unwrap().unwrap();
        worker.finish().await.unwrap();
    }

    #[test]
    fn adaptive_target_obeys_ewma_deadline_and_update_limits_without_eviction() {
        let now = Instant::now();
        let mut adaptive = AdaptiveTarget::new(
            Config {
                max_inflight: 8,
                queue_timeout: Duration::from_secs(1),
                max_connections: 16,
            },
            now,
        );
        assert_eq!(
            adaptive.observe(Duration::from_secs(2), now + Duration::from_millis(999)),
            8
        );
        assert_eq!(
            adaptive.observe(Duration::from_secs(2), now + Duration::from_secs(1)),
            1
        );
        assert_eq!(adaptive.ewma_ns, Some(2_000_000_000));
        for _ in 0..100 {
            adaptive.observe(Duration::ZERO, now + Duration::from_millis(1999));
        }
        assert_eq!(adaptive.target, 1);
        assert_eq!(
            adaptive.observe(Duration::ZERO, now + Duration::from_secs(2)),
            2
        );
        assert_eq!(
            adaptive.observe(Duration::ZERO, now + Duration::from_secs(2)),
            2
        );
        for second in 3..=10 {
            adaptive.observe(Duration::ZERO, now + Duration::from_secs(second));
        }
        assert_eq!(adaptive.target, 8);
        assert_eq!(
            adaptive.observe(Duration::from_secs(80), now + Duration::from_secs(11)),
            1
        );
    }

    #[tokio::test]
    async fn lowering_target_keeps_already_accepted_commands() {
        let database = TestStore::new();
        let (executor, worker) = database.executor(3);
        let (release, first) = blocked(&executor).await;
        let second = tokio::spawn({
            let executor = executor.clone();
            async move { executor.run(|_| Ok(json!(2))).await }
        });
        queued(&executor, 1).await;
        // Simulate the controller's decrease while two commands are accepted.
        executor.0.gate.lock().unwrap().target = 1;
        assert!(executor
            .run(|_| panic!("admitted above target"))
            .await
            .is_err());
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        assert_eq!(second.await.unwrap().unwrap(), json!(2));
        worker.finish().await.unwrap();
    }
}
