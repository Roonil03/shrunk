//! Experimental Unix-only decode/filter pipeline. A caller folds matches in
//! original order; worker threads never aggregate floating-point values.

use super::{Block, Error, Point, Predicate, Result};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

#[cfg(feature = "experimental-parallel-aggregate")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "experimental-parallel-aggregate")]
use std::sync::OnceLock;

const MAX_WORKERS: usize = 4;
const MAX_BATCH_BLOCKS: usize = 8;
// Each submitted job occupies one worker slot, including its undrained output.
// Estimated input, decoder scratch, and decoded rows across slots total at
// most 16 MiB (plus small channel, container, and OS thread-stack overhead).
const SLOT_BYTES: usize = 4 * 1024 * 1024;
const CODEC_SCRATCH_BYTES: usize = 64 * 1024;
#[cfg(feature = "experimental-parallel-aggregate")]
const MIN_OVERLAP_BLOCKS: usize = 16;
#[cfg(feature = "experimental-parallel-aggregate")]
const MIN_ESTIMATED_POINTS: u64 = 128 * 1024;

#[cfg(feature = "experimental-parallel-aggregate")]
static PARALLEL_AGGREGATE_BUSY: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "experimental-parallel-aggregate")]
struct Lease;

#[cfg(feature = "experimental-parallel-aggregate")]
impl Lease {
    fn try_acquire() -> Option<Self> {
        PARALLEL_AGGREGATE_BUSY
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self)
    }
}

#[cfg(feature = "experimental-parallel-aggregate")]
impl Drop for Lease {
    fn drop(&mut self) {
        PARALLEL_AGGREGATE_BUSY.store(false, Ordering::Release);
    }
}

#[derive(Default)]
pub(super) struct FilterStats {
    pub visited: usize,
    pub skipped: usize,
    pub decoded: usize,
    pub points: usize,
    pub bytes: usize,
}

impl FilterStats {
    fn add(&mut self, other: Self) {
        self.visited += other.visited;
        self.skipped += other.skipped;
        self.decoded += other.decoded;
        self.points += other.points;
        self.bytes += other.bytes;
    }
}

pub(super) fn try_fold(
    blocks: &[Block],
    start: i64,
    end: i64,
    predicate: Predicate,
    _force_for_test: bool,
    fold: impl FnMut(Point),
) -> Result<Option<FilterStats>> {
    let stop = blocks.partition_point(|block| block.t_min <= end);
    let blocks = &blocks[..stop];
    #[cfg(test)]
    if _force_for_test {
        let result = run(blocks, start, end, predicate, 2, fold)?;
        assert!(result.is_some(), "could not start test workers");
        return Ok(result);
    }
    #[cfg(feature = "experimental-parallel-aggregate")]
    {
        let Some(workers) = eligible_workers(blocks, predicate) else {
            return Ok(None);
        };
        // The service already serializes database work; a process-wide
        // lease also prevents library callers multiplying decode pools.
        let Some(_lease) = Lease::try_acquire() else {
            return Ok(None);
        };
        run(blocks, start, end, predicate, workers, fold)
    }
    #[cfg(not(feature = "experimental-parallel-aggregate"))]
    {
        Ok(None)
    }
}

#[cfg(feature = "experimental-parallel-aggregate")]
fn eligible_workers(blocks: &[Block], predicate: Predicate) -> Option<usize> {
    if blocks.len() < MIN_OVERLAP_BLOCKS {
        return None;
    }
    // A bounded, spread-out probe avoids scanning a long mostly-skipped
    // range twice. A false negative only loses the optional acceleration.
    let samples = blocks.len().min(32);
    let mut points = 0u64;
    let mut useful = 0usize;
    let mut probe = 0x8a5c_a4c1u32;
    for i in 0..samples {
        let (lo, hi) = (i * blocks.len() / samples, (i + 1) * blocks.len() / samples);
        // Vary position within each stratum so periodic bursts do not all
        // happen to line up with regularly spaced probe positions.
        probe = probe.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let block = &blocks[lo + (probe as usize % (hi - lo))];

        if estimated_bytes(block) <= SLOT_BYTES && predicate.may_match(block) {
            points += u64::from(block.count);
            useful += 1;
        }
    }
    if useful < 2 || points * (blocks.len() as u64) / (samples as u64) < MIN_ESTIMATED_POINTS {
        return None;
    }
    static WORKERS: OnceLock<usize> = OnceLock::new();
    let workers = *WORKERS.get_or_init(|| {
        thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1).min(MAX_WORKERS))
            .unwrap_or(0)
    });
    (workers >= 2).then_some(workers)
}

fn estimated_bytes(block: &Block) -> usize {
    // Reserve the decoded vector even if all values are rejected, plus
    // compressed file input, separate codec timestamp/value vectors and
    // the two 4096-entry predictor tables used by the current codecs.
    let bytes_per_point =
        std::mem::size_of::<Point>() + std::mem::size_of::<i64>() + std::mem::size_of::<f64>();
    (block.count as usize)
        .saturating_mul(bytes_per_point)
        .saturating_add(block.payload.len())
        .saturating_add(CODEC_SCRATCH_BYTES)
}

fn batch_len(blocks: &[Block]) -> usize {
    let mut bytes = 0usize;
    blocks
        .iter()
        .take(MAX_BATCH_BLOCKS)
        .take_while(|block| {
            bytes = bytes.saturating_add(estimated_bytes(block));
            bytes <= SLOT_BYTES
        })
        .count()
}

struct Batch {
    stats: FilterStats,
    matches: Vec<Vec<Point>>,
}

fn decode(blocks: &[Block], start: i64, end: i64, predicate: Predicate) -> Result<Batch> {
    let mut stats = FilterStats::default();
    let mut matches = Vec::with_capacity(blocks.len());
    for block in blocks {
        stats.visited += 1;
        if !predicate.may_match(block) {
            stats.skipped += 1;
            continue;
        }
        let mut points = block.decode().map_err(Error::Format)?;
        stats.decoded += 1;
        stats.points += points.len();
        stats.bytes += block.payload.len();
        points.retain(|p| p.ts >= start && p.ts <= end && predicate.matches(p.value));
        if !points.is_empty() {
            matches.push(points);
        }
    }
    Ok(Batch { stats, matches })
}

struct Worker<'a> {
    jobs: SyncSender<&'a [Block]>,
    results: Receiver<Result<Batch>>,
}

fn channel_closed() -> Error {
    Error::Io(std::io::Error::other("parallel aggregate worker stopped"))
}

fn run(
    blocks: &[Block],
    start: i64,
    end: i64,
    predicate: Predicate,
    workers: usize,
    mut fold: impl FnMut(Point),
) -> Result<Option<FilterStats>> {
    debug_assert!((2..=MAX_WORKERS).contains(&workers));
    thread::scope(|scope| {
        let mut pool = Vec::with_capacity(workers);
        for _ in 0..workers {
            let (jobs, work) = mpsc::sync_channel(1);
            let (result, results) = mpsc::sync_channel(1);
            if thread::Builder::new()
                .spawn_scoped(scope, move || {
                    while let Ok(batch) = work.recv() {
                        if result.send(decode(batch, start, end, predicate)).is_err() {
                            break;
                        }
                    }
                })
                .is_err()
            {
                // No jobs are submitted until the entire pool starts. The
                // existing serial path still owns the full operation.
                return Ok(None);
            }
            pool.push(Worker { jobs, results });
        }

        let mut stats = FilterStats::default();
        let mut collect = |batch: Batch| {
            stats.add(batch.stats);
            for point in batch.matches.into_iter().flatten() {
                fold(point);
            }
        };
        let mut remaining = blocks;
        while !remaining.is_empty() {
            let mut submitted = 0;
            for worker in &pool {
                let len = batch_len(remaining);
                if len == 0 {
                    break;
                }
                let (batch, tail) = remaining.split_at(len);
                worker.jobs.send(batch).map_err(|_| channel_closed())?;
                remaining = tail;
                submitted += 1;
            }
            if submitted == 0 {
                // Large individual blocks get the original serial allocation
                // budget. No other jobs are in flight during this decode.
                let (block, tail) = remaining.split_at(1);
                collect(decode(block, start, end, predicate)?);
                remaining = tail;
            } else {
                // Drain in input order. One job per worker per round bounds
                // out-of-order buffering and retains the first logical error.
                for worker in &pool[..submitted] {
                    collect(worker.results.recv().map_err(|_| channel_closed())??);
                }
            }
        }
        Ok(Some(stats))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Agg, AggResult, Db, QueryStats};
    use std::fs::{self, OpenOptions};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;

    fn assert_same(left: AggResult, right: AggResult) {
        assert_eq!(left.value.map(f64::to_bits), right.value.map(f64::to_bits));
        fn stats(s: QueryStats) -> [usize; 10] {
            [
                s.blocks_total,
                s.blocks_visited,
                s.blocks_decoded,
                s.blocks_skipped_time,
                s.blocks_skipped_value,
                s.blocks_skipped_limit,
                s.fast_blocks,
                s.points_decoded,
                s.pending_points,
                s.bytes_decoded,
            ]
        }
        assert_eq!(stats(left.stats), stats(right.stats));
    }

    fn fixture() -> Db {
        let mut db = Db::with_block_size(8);
        let values = [
            1e16,
            1.0,
            -1e16,
            -0.0,
            0.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ];
        for i in 0..517 {
            let value = if i / 8 % 5 == 2 {
                -20.0
            } else {
                values[(i * 3 + i / 8) % values.len()]
            };
            db.write("s", i as i64 - 250, value).unwrap();
        }
        db
    }

    fn private_temp() -> std::path::PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shrunk-parallel-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn parallel_matches_serial_value_bits_and_all_stats() {
        let mut db = fixture();
        assert_eq!(db.series["s"].pending.len(), 5);
        let dir = private_temp();
        let path = dir.join("numeric.shrunk");
        for file_backed in [false, true] {
            if file_backed {
                db.save(&path).unwrap();
                db = Db::open(&path).unwrap();
            }
            for agg in [Agg::Count, Agg::Sum, Agg::Avg, Agg::Min, Agg::Max] {
                for predicate in [
                    Some(Predicate::Ge(-1e16)),
                    Some(Predicate::Ne(f64::NAN)),
                    Some(Predicate::Eq(-20.0)),
                    None,
                ] {
                    for range in [
                        i64::MIN..=i64::MAX,
                        -101..=201,
                        255..=266,
                        std::ops::RangeInclusive::new(5, 4),
                    ] {
                        let serial = db
                            .aggregate_serial("s", *range.start(), *range.end(), agg, predicate)
                            .unwrap();
                        let parallel = if let Some(pred) = predicate {
                            db.try_aggregate_parallel("s", range.clone(), agg, pred, true)
                                .unwrap()
                        } else {
                            None
                        };
                        let parallel = parallel.unwrap_or_else(|| {
                            db.aggregate("s", *range.start(), *range.end(), agg, predicate)
                                .unwrap()
                        });
                        assert_same(serial, parallel);
                    }
                }
            }
        }
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn finds_first_read_error_in_block_order() {
        let dir = private_temp();
        let path = dir.join("write-only-handle");
        let file = Arc::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap(),
        );
        let mut db = fixture();
        let series = db.series.get_mut("s").unwrap();
        for (index, offset) in [(1, 107), (MAX_BATCH_BLOCKS, 211)] {
            let block = &mut series.blocks[index];
            block.payload = super::super::Payload::File {
                file: Arc::clone(&file),
                offset,
                len: block.payload.len() as u32,
            };
        }
        let pred = Predicate::Ne(f64::NAN);
        let serial_error = db
            .aggregate_serial("s", i64::MIN, i64::MAX, Agg::Count, Some(pred))
            .unwrap_err();
        let parallel_error = db
            .try_aggregate_parallel("s", i64::MIN..=i64::MAX, Agg::Count, pred, true)
            .unwrap_err();
        assert!(matches!(serial_error, Error::Format(message) if message.contains("at 107")));
        assert!(matches!(parallel_error, Error::Format(message) if message.contains("at 107")));
        drop(db);
        drop(file);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn planner_bounds_batch_count_and_estimated_bytes() {
        let db = fixture();
        let blocks = &db.series["s"].blocks;
        assert_eq!(batch_len(blocks), MAX_BATCH_BLOCKS);
        let mut larger = blocks[..MAX_BATCH_BLOCKS].to_vec();
        for block in &mut larger {
            block.count = 48_000;
        }
        let len = batch_len(&larger);
        assert!(len > 0 && len < larger.len());
        assert!(larger[..len].iter().map(estimated_bytes).sum::<usize>() <= SLOT_BYTES);
        larger[0].count = 200_000;
        assert_eq!(batch_len(&larger), 0);
    }

    #[cfg(feature = "experimental-parallel-aggregate")]
    #[test]
    fn small_and_predicate_skipped_queries_do_not_open_a_pool() {
        let db = fixture();
        let blocks = &db.series["s"].blocks;
        let mut skipped = blocks.clone();
        for block in &mut skipped {
            block.count = 4096;
            block.has_nan = false;
            block.v_min = -20.0;
            block.v_max = -20.0;
        }
        assert_eq!(eligible_workers(&skipped, Predicate::Gt(0.0)), None);
        assert_eq!(
            eligible_workers(&blocks[..2], Predicate::Ne(f64::NAN)),
            None
        );
    }

    #[cfg(feature = "experimental-parallel-aggregate")]
    #[test]
    fn one_process_wide_slot_is_released() {
        let lease = Lease::try_acquire().unwrap();
        assert!(std::thread::spawn(|| Lease::try_acquire().is_none())
            .join()
            .unwrap());
        drop(lease);
        assert!(Lease::try_acquire().is_some());
    }
}
