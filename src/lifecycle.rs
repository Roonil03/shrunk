//! Explicit maintenance and fixed-width aggregation. Snapshot and WAL formats
//! stay unchanged; durable destructive maintenance checkpoints the old WAL
//! before publishing a replacement snapshot.

use super::*;

/// Work performed by range deletion or retention.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteResult {
    pub points_deleted: u64,
    /// Whole compressed blocks removed without decoding their payloads.
    pub blocks_dropped: usize,
    /// Partially overlapping blocks decoded and rebuilt.
    pub blocks_rewritten: usize,
}

impl DeleteResult {
    fn add(&mut self, other: Self) {
        self.points_deleted += other.points_deleted;
        self.blocks_dropped += other.blocks_dropped;
        self.blocks_rewritten += other.blocks_rewritten;
    }
}

impl Db {
    /// The earliest surviving timestamp across all series, using only block
    /// metadata and pending points. Empty databases return `None`.
    pub fn oldest_timestamp(&self) -> Option<i64> {
        self.series.values().filter_map(series_oldest).min()
    }

    /// Remove an inclusive time range. An inverted range is a no-op. The
    /// series remains present even if all its points are removed. Later writes
    /// must increase beyond the last surviving timestamp.
    ///
    /// On a WAL-bound database this is durable when it returns successfully:
    /// the old WAL is checkpointed before the replacement snapshot is written.
    /// Other handles change only in memory until explicitly saved.
    pub fn delete_range(&mut self, series: &str, start: i64, end: i64) -> Result<DeleteResult> {
        self.ensure_snapshot_dir_durable()?;
        let existing = self
            .series
            .get(series)
            .ok_or_else(|| Error::UnknownSeries(series.to_string()))?;
        if start > end {
            return Ok(DeleteResult::default());
        }
        let (replacement, result) = deleted_series(existing, start, end, self.block_size)?;
        if result.points_deleted > 0 {
            self.install_series(vec![(series.to_string(), Some(replacement))])?;
        }
        Ok(result)
    }

    /// Drop a series and return its former point count. Durability follows
    /// [`Db::delete_range`]. A missing series returns `Error::UnknownSeries`.
    pub fn delete_series(&mut self, series: &str) -> Result<u64> {
        let count = self
            .series
            .get(series)
            .ok_or_else(|| Error::UnknownSeries(series.to_string()))?
            .point_count();
        self.install_series(vec![(series.to_string(), None)])?;
        Ok(count)
    }

    /// Keep timestamps at or after `cutoff` across all series, atomically.
    /// Empty series remain present. This is an explicit maintenance operation;
    /// it does not configure a policy for future writes.
    pub fn retain_since(&mut self, cutoff: i64) -> Result<DeleteResult> {
        self.ensure_snapshot_dir_durable()?;
        if cutoff == i64::MIN {
            return Ok(DeleteResult::default());
        }
        let mut changes = Vec::new();
        let mut total = DeleteResult::default();
        for (name, series) in &self.series {
            // Periodic retention commonly expires nothing. Avoid cloning
            // compressed payloads or scanning the block index in that case.
            if series_oldest(series).is_none_or(|oldest| oldest >= cutoff) {
                continue;
            }
            let (replacement, result) =
                deleted_series(series, i64::MIN, cutoff - 1, self.block_size)?;
            if result.points_deleted > 0 {
                total.add(result);
                changes.push((name.clone(), Some(replacement)));
            }
        }
        if !changes.is_empty() {
            self.install_series(changes)?;
        }
        Ok(total)
    }

    /// Keep timestamps at or after `now - window_ms` across all series.
    /// The window must be positive; subtraction saturates at `i64::MIN`.
    /// Future-dated points are retained. No wall clock is consulted.
    pub fn retain_window(&mut self, now: i64, window_ms: i64) -> Result<DeleteResult> {
        validate_interval(window_ms)?;
        self.retain_since(now.saturating_sub(window_ms))
    }

    /// Group points in an inclusive range into epoch-aligned, half-open
    /// buckets of `interval_ms`, using the existing aggregate NaN semantics.
    /// Empty buckets and buckets whose aggregate is undefined are omitted.
    /// A result timestamp is the bucket start, which can precede `start`.
    /// The first representable bucket start saturates at `i64::MIN`.
    /// Full blocks inside one bucket use metadata whenever possible.
    pub fn downsample(
        &self,
        series: &str,
        start: i64,
        end: i64,
        interval_ms: i64,
        agg: Agg,
    ) -> Result<QueryResult> {
        self.downsample_impl(series, start, end, interval_ms, agg, usize::MAX)
    }

    /// Return at most `limit` completed buckets, stopping before scanning the
    /// rest of the series. This bounds output allocation during execution.
    /// Use one extra bucket to detect truncation when paginating results.
    /// Limits above the database query budget are rejected; zero returns no
    /// buckets. Bucket contents and alignment follow [`Db::downsample`].
    pub fn downsample_with_limit(
        &self,
        series: &str,
        start: i64,
        end: i64,
        interval_ms: i64,
        agg: Agg,
        limit: usize,
    ) -> Result<QueryResult> {
        if limit > MAX_QUERY_POINTS {
            return Err(Error::Limit(format!(
                "downsample limit exceeds {MAX_QUERY_POINTS} buckets"
            )));
        }
        self.downsample_impl(series, start, end, interval_ms, agg, limit)
    }

    fn downsample_impl(
        &self,
        series: &str,
        start: i64,
        end: i64,
        interval_ms: i64,
        agg: Agg,
        limit: usize,
    ) -> Result<QueryResult> {
        validate_interval(interval_ms)?;
        let s = self
            .series
            .get(series)
            .ok_or_else(|| Error::UnknownSeries(series.to_string()))?;
        let mut stats = QueryStats {
            blocks_total: s.blocks.len(),
            pending_points: s.pending.len(),
            ..Default::default()
        };
        let mut output = Buckets::new(interval_ms, agg);
        let mut stopped = false;
        if start <= end && limit > 0 {
            let first = s.blocks.partition_point(|b| b.t_max < start);
            stats.blocks_skipped_time = first;
            for (index, b) in s.blocks.iter().enumerate().skip(first) {
                if b.t_min > end {
                    stats.blocks_skipped_time += s.blocks.len() - index;
                    break;
                }
                stats.blocks_visited += 1;
                if b.t_min >= start
                    && b.t_max <= end
                    && bucket_start(b.t_min, interval_ms) == bucket_start(b.t_max, interval_ms)
                    && output.block(b)?
                {
                    stats.fast_blocks += 1;
                    if output.points.len() >= limit {
                        stopped = true;
                        break;
                    }
                    continue;
                }
                let points = b.decode().map_err(Error::Format)?;
                stats.blocks_decoded += 1;
                stats.points_decoded += points.len();
                stats.bytes_decoded += b.payload.len();
                for p in points {
                    if p.ts >= start && p.ts <= end {
                        output.point(p)?;
                        if output.points.len() >= limit {
                            stopped = true;
                            break;
                        }
                    }
                }
                if stopped {
                    break;
                }
            }
            if !stopped {
                for p in &s.pending {
                    if p.ts > end {
                        break;
                    }
                    if p.ts >= start {
                        output.point(*p)?;
                        if output.points.len() >= limit {
                            stopped = true;
                            break;
                        }
                    }
                }
            }
        } else if limit == 0 {
            stats.blocks_skipped_limit = stats.blocks_total;
        } else {
            stats.blocks_skipped_time = stats.blocks_total;
        }
        stats.blocks_skipped_limit = stats
            .blocks_total
            .saturating_sub(stats.blocks_skipped_time + stats.blocks_visited);
        if !stopped {
            output.finish()?;
        }
        Ok(QueryResult {
            points: output.points,
            stats,
        })
    }

    /// Materialize a downsample into a new, distinct series. Existing targets
    /// are rejected, including empty targets. The source is unchanged. The
    /// entire result is installed atomically, with deletion's durability rules.
    /// This is a one-time rollup, not an automatically maintained view.
    pub fn rollup(
        &mut self,
        source: &str,
        target: &str,
        start: i64,
        end: i64,
        interval_ms: i64,
        agg: Agg,
    ) -> Result<u64> {
        self.rollup_with_limit(
            source,
            target,
            start..=end,
            interval_ms,
            agg,
            MAX_QUERY_POINTS,
        )
    }

    /// Materialize a complete rollup within an output budget. If more than
    /// `max_buckets` would be produced, return `Error::Limit` before modifying
    /// the database. Partial rollups are never installed. The maximum budget
    /// is the database query limit; zero is rejected.
    pub fn rollup_with_limit(
        &mut self,
        source: &str,
        target: &str,
        range: std::ops::RangeInclusive<i64>,
        interval_ms: i64,
        agg: Agg,
        max_buckets: usize,
    ) -> Result<u64> {
        if max_buckets == 0 || max_buckets > MAX_QUERY_POINTS {
            return Err(Error::InvalidInput(format!(
                "rollup bucket limit must be in 1..={MAX_QUERY_POINTS}"
            )));
        }
        if source == target || self.series.contains_key(target) {
            return Err(Error::InvalidInput(
                "rollup target must be a new series distinct from the source".into(),
            ));
        }
        validate_series_write(target, true, self.series.len())?;
        let result = self.downsample_impl(
            source,
            *range.start(),
            *range.end(),
            interval_ms,
            agg,
            max_buckets.saturating_add(1),
        )?;
        if result.points.len() > max_buckets {
            return Err(Error::Limit(format!(
                "rollup exceeds {max_buckets} bucket limit"
            )));
        }
        let count = result.points.len() as u64;
        let mut replacement = Series::new(target);
        replacement.last_ts = result.points.last().map(|p| p.ts);
        for chunk in result.points.chunks(self.block_size) {
            replacement.blocks.push(Block::build(chunk));
        }
        self.install_series(vec![(target.to_string(), Some(replacement))])?;
        Ok(count)
    }

    fn install_series(&mut self, changes: Vec<(String, Option<Series>)>) -> Result<()> {
        self.install_series_with_hook(changes, |_| Ok(()))
    }

    fn install_series_with_hook(
        &mut self,
        changes: Vec<(String, Option<Series>)>,
        hook: impl FnMut(SaveStage) -> Result<()>,
    ) -> Result<()> {
        self.ensure_snapshot_dir_durable()?;
        let mut projected = self.projected_blocks;
        for (name, replacement) in &changes {
            if let Some(old) = self.series.get(name) {
                projected -= old.blocks.len() + old.flush_block_delta(self.block_size);
            }
            if let Some(new) = replacement {
                projected = projected
                    .checked_add(new.blocks.len() + new.flush_block_delta(self.block_size))
                    .ok_or_else(|| Error::Limit("snapshot block count overflow".into()))?;
            }
        }
        self.ensure_block_budget(projected)?;
        // Timestamp-based WAL replay cannot distinguish removed tail points
        // from new appends. Make the old WAL durably empty before publishing
        // any removal, while retaining the exclusive writer lock throughout.
        if self.wal.is_some() {
            self.checkpoint()?;
        }
        let old_projected = self.projected_blocks;
        let mut originals = Vec::with_capacity(changes.len());
        for (name, replacement) in changes {
            let old = match replacement {
                Some(new) => self.series.insert(name.clone(), new),
                None => self.series.remove(&name),
            };
            originals.push((name, old));
        }
        self.projected_blocks = projected;
        if self.wal.is_some() {
            let path = self.path.clone().unwrap();
            let mut published = false;
            let result = self.save_with_status(&path, hook, &mut published);
            if result.is_err() && !published {
                for (name, old) in originals {
                    match old {
                        Some(old) => {
                            self.series.insert(name, old);
                        }
                        None => {
                            self.series.remove(&name);
                        }
                    }
                }
                self.projected_blocks = old_projected;
            }
            return result;
        }
        Ok(())
    }
}

fn series_oldest(series: &Series) -> Option<i64> {
    series
        .blocks
        .first()
        .map(|b| b.t_min)
        .or_else(|| series.pending.first().map(|p| p.ts))
}

fn deleted_series(
    original: &Series,
    start: i64,
    end: i64,
    block_size: usize,
) -> Result<(Series, DeleteResult)> {
    let mut replacement = Series::new(&original.name);
    let mut result = DeleteResult::default();
    for b in &original.blocks {
        if b.t_max < start || b.t_min > end {
            replacement.blocks.push(b.clone());
        } else if b.t_min >= start && b.t_max <= end {
            result.points_deleted += b.count as u64;
            result.blocks_dropped += 1;
        } else {
            let points = b.decode().map_err(Error::Format)?;
            let kept: Vec<Point> = points
                .iter()
                .copied()
                .filter(|p| p.ts < start || p.ts > end)
                .collect();
            result.points_deleted += (points.len() - kept.len()) as u64;
            result.blocks_rewritten += 1;
            for chunk in kept.chunks(block_size) {
                replacement.blocks.push(Block::build(chunk));
            }
        }
    }
    replacement.pending = original
        .pending
        .iter()
        .copied()
        .filter(|p| p.ts < start || p.ts > end)
        .collect();
    result.points_deleted += (original.pending.len() - replacement.pending.len()) as u64;
    replacement.last_ts = replacement
        .pending
        .last()
        .map(|p| p.ts)
        .or_else(|| replacement.blocks.last().map(|b| b.t_max));
    Ok((replacement, result))
}

fn validate_interval(interval: i64) -> Result<()> {
    if interval <= 0 {
        return Err(Error::InvalidInput(
            "interval/window must be positive".into(),
        ));
    }
    Ok(())
}

fn bucket_start(ts: i64, interval: i64) -> i128 {
    i128::from(ts).div_euclid(i128::from(interval)) * i128::from(interval)
}

#[derive(Default)]
struct Bucket {
    start: i128,
    count: u64,
    sum: f64,
    extreme: Option<f64>,
    matched: bool,
}

struct Buckets {
    interval: i64,
    agg: Agg,
    current: Option<Bucket>,
    points: Vec<Point>,
}

impl Buckets {
    fn new(interval: i64, agg: Agg) -> Self {
        Self {
            interval,
            agg,
            current: None,
            points: Vec::new(),
        }
    }

    fn at(&mut self, ts: i64) -> Result<&mut Bucket> {
        let start = bucket_start(ts, self.interval);
        if self.current.as_ref().is_some_and(|b| b.start != start) {
            self.finish()?;
        }
        Ok(self.current.get_or_insert_with(|| Bucket {
            start,
            ..Default::default()
        }))
    }

    fn point(&mut self, p: Point) -> Result<()> {
        let agg = self.agg;
        let b = self.at(p.ts)?;
        fold(
            agg,
            p.value,
            &mut b.count,
            &mut b.sum,
            &mut b.extreme,
            &mut b.matched,
        );
        Ok(())
    }

    fn block(&mut self, block: &Block) -> Result<bool> {
        let agg = self.agg;
        if !match agg {
            Agg::Count => true,
            Agg::Sum | Agg::Avg => block.sum.is_some(),
            Agg::Min | Agg::Max => !block.has_nan,
        } {
            return Ok(false);
        }
        let b = self.at(block.t_min)?;
        b.matched = true;
        match agg {
            Agg::Count => b.count += u64::from(block.count),
            Agg::Sum | Agg::Avg => {
                b.count += u64::from(block.count);
                b.sum += block.sum.unwrap();
            }
            Agg::Min => b.extreme = Some(b.extreme.map_or(block.v_min, |v| v.min(block.v_min))),
            Agg::Max => b.extreme = Some(b.extreme.map_or(block.v_max, |v| v.max(block.v_max))),
        }
        Ok(true)
    }

    fn finish(&mut self) -> Result<()> {
        let Some(b) = self.current.take() else {
            return Ok(());
        };
        let value = match self.agg {
            Agg::Count if b.matched => Some(b.count as f64),
            Agg::Sum if b.matched => Some(b.sum),
            Agg::Avg if b.count > 0 => Some(b.sum / b.count as f64),
            Agg::Min | Agg::Max => b.extreme,
            _ => None,
        };
        if let Some(value) = value {
            if self.points.len() >= MAX_QUERY_POINTS {
                return Err(Error::Limit(format!(
                    "downsample exceeds {MAX_QUERY_POINTS} bucket limit"
                )));
            }
            if self.points.len() == self.points.capacity() {
                self.points
                    .try_reserve(1024)
                    .map_err(|_| Error::Limit("downsample allocation failed".into()))?;
            }
            self.points
                .push(Point::new(b.start.max(i128::from(i64::MIN)) as i64, value));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_snapshot_failures_preserve_the_published_generation() {
        for (sequence, stage) in [
            SaveStage::Write,
            SaveStage::FileSync,
            SaveStage::Rename,
            SaveStage::DirectorySync,
            SaveStage::Truncate,
        ]
        .into_iter()
        .enumerate()
        {
            let dir = std::env::temp_dir().join(format!(
                "shrunk-lifecycle-fault-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir(&dir).unwrap();
            let path = dir.join("test.db");
            let original: Vec<_> = (0..8).map(|ts| Point::new(ts, ts as f64)).collect();
            let mut db = Db::open_rw_with_block_size(&path, 3).unwrap();
            db.append("s", &original).unwrap();
            let (replacement, _) = deleted_series(&db.series["s"], 4, 7, 3).unwrap();
            let result = db.install_series_with_hook(vec![("s".into(), Some(replacement))], |at| {
                if at == stage {
                    Err(Error::Io(std::io::Error::other(
                        "injected lifecycle failure",
                    )))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err(), "stage {stage:?}");
            let published = matches!(stage, SaveStage::DirectorySync | SaveStage::Truncate);
            let expected = if published {
                &original[..4]
            } else {
                &original[..]
            };
            for handle in [&db, &Db::open(&path).unwrap()] {
                assert_eq!(
                    handle
                        .query(&Query::new("s", i64::MIN, i64::MAX))
                        .unwrap()
                        .points,
                    expected,
                    "stage {stage:?}"
                );
            }
            // A retry or append after an error must use the visible generation.
            let next = Point::new(expected.last().unwrap().ts + 1, 999.0);
            if stage == SaveStage::DirectorySync {
                assert!(db.pending_snapshot_dir_sync.is_some());
                let wal_path = wal::wal_path(&path);
                let wal_len = fs::metadata(&wal_path).unwrap().len();
                db.fail_snapshot_dir_sync = true;
                assert!(matches!(db.append("s", &[next]), Err(Error::Io(_))));
                assert_eq!(fs::metadata(&wal_path).unwrap().len(), wal_len);
                // Identical maintenance must not report a successful no-op
                // while the preceding replacement remains uncommitted.
                assert!(matches!(db.delete_range("s", 4, 7), Err(Error::Io(_))));
                assert!(matches!(db.retain_since(i64::MIN), Err(Error::Io(_))));
                db.fail_snapshot_dir_sync = false;
                assert_eq!(db.delete_range("s", 4, 7).unwrap().points_deleted, 0);
                assert_eq!(db.retain_since(i64::MIN).unwrap().points_deleted, 0);
                assert!(db.pending_snapshot_dir_sync.is_none());
            }
            db.append("s", &[next]).unwrap();
            drop(db);
            let reopened = Db::open_rw(&path).unwrap();
            assert_eq!(
                reopened
                    .query(&Query::new("s", i64::MIN, i64::MAX))
                    .unwrap()
                    .points,
                [expected, &[next]].concat(),
                "stage {stage:?}"
            );
            drop(reopened);
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn reopened_writer_barriers_a_prior_unsynced_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "shrunk-lifecycle-reopen-barrier-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let path = dir.join("test.db");
        let mut db = Db::open_rw_with_block_size(&path, 3).unwrap();
        let original: Vec<_> = (0..8).map(|ts| Point::new(ts, ts as f64)).collect();
        db.append("s", &original).unwrap();
        let (replacement, _) = deleted_series(&db.series["s"], 4, 7, 3).unwrap();
        assert!(db
            .install_series_with_hook(vec![("s".into(), Some(replacement))], |at| {
                if at == SaveStage::DirectorySync {
                    Err(Error::Io(std::io::Error::other(
                        "injected directory sync failure",
                    )))
                } else {
                    Ok(())
                }
            })
            .is_err());
        drop(db);

        assert!(matches!(
            Db::open_rw_with_block_size_and_hook(&path, 3, || Err(Error::Io(
                std::io::Error::other("injected reopen barrier failure")
            ))),
            Err(Error::Io(_))
        ));
        let mut reopened = Db::open_rw(&path).unwrap();
        reopened.append("s", &[Point::new(4, 99.0)]).unwrap();
        drop(reopened);
        let points = Db::open(&path)
            .unwrap()
            .query(&Query::new("s", 0, 7))
            .unwrap()
            .points;
        assert_eq!(points.len(), 5);
        assert_eq!(points.last(), Some(&Point::new(4, 99.0)));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn identical_retention_retry_waits_for_directory_sync() {
        let dir = std::env::temp_dir().join(format!(
            "shrunk-lifecycle-retain-barrier-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let path = dir.join("test.db");
        let mut db = Db::open_rw_with_block_size(&path, 3).unwrap();
        db.append(
            "s",
            &(0..8)
                .map(|ts| Point::new(ts, ts as f64))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let (replacement, _) = deleted_series(&db.series["s"], i64::MIN, 3, 3).unwrap();
        assert!(db
            .install_series_with_hook(vec![("s".into(), Some(replacement))], |at| {
                if at == SaveStage::DirectorySync {
                    Err(Error::Io(std::io::Error::other(
                        "injected directory sync failure",
                    )))
                } else {
                    Ok(())
                }
            })
            .is_err());
        db.fail_snapshot_dir_sync = true;
        assert!(matches!(db.retain_since(4), Err(Error::Io(_))));
        db.fail_snapshot_dir_sync = false;
        assert_eq!(db.retain_since(4).unwrap().points_deleted, 0);
        assert_eq!(db.point_count(), 4);
        drop(db);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn oversized_rollup_is_rejected_before_snapshot_publish() {
        let dir = std::env::temp_dir().join(format!(
            "shrunk-lifecycle-size-limit-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let path = dir.join("test.db");
        let mut db = Db::open_rw_with_block_size(&path, 3).unwrap();
        let original: Vec<_> = (0..8).map(|ts| Point::new(ts, ts as f64)).collect();
        db.append("s", &original).unwrap();
        db.checkpoint().unwrap();
        let before = fs::read(&path).unwrap();
        db.snapshot_byte_limit = before.len() as u64;

        assert!(matches!(
            db.rollup("s", "r", 0, 7, 2, Agg::Count),
            Err(Error::Limit(_))
        ));
        assert_eq!(db.series_names(), vec!["s"]);
        assert_eq!(db.point_count(), original.len() as u64);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(Db::open(&path).unwrap().series_names(), vec!["s"]);
        db.snapshot_byte_limit = MAX_DB_BYTES;
        assert_eq!(db.rollup("s", "r", 0, 7, 2, Agg::Count).unwrap(), 4);
        assert_eq!(Db::open(&path).unwrap().series_names(), vec!["r", "s"]);
        drop(db);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_boundary_decode_leaves_all_series_unchanged() {
        let mut db = Db::with_block_size(4);
        db.write_many("agood", &[Point::new(0, 0.0), Point::new(1, 1.0)])
            .unwrap();
        db.write_many(
            "zbad",
            &(0..8)
                .map(|ts| Point::new(ts, ts as f64))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let block = &mut db.series.get_mut("zbad").unwrap().blocks[0];
        block.payload_crc = Some(0);
        assert!(matches!(db.retain_since(2), Err(Error::Format(_))));
        assert_eq!(db.point_count(), 10);
        assert_eq!(db.series["agood"].point_count(), 2);
        assert_eq!(db.series["zbad"].point_count(), 8);
    }

    #[test]
    fn deletion_keeps_untouched_compressed_payloads_byte_for_byte() {
        let mut db = Db::with_block_size(4);
        let points: Vec<_> = (0..20).map(|ts| Point::new(ts, (ts * ts) as f64)).collect();
        db.write_many("s", &points).unwrap();
        let first = db.series["s"].blocks[0].clone();
        let last = db.series["s"].blocks[4].clone();
        db.delete_range("s", 5, 14).unwrap();
        let series = &db.series["s"];
        for (before, after) in [
            (&first, &series.blocks[0]),
            (&last, series.blocks.last().unwrap()),
        ] {
            assert_eq!(before.codec, after.codec);
            assert_eq!(
                before.payload.bytes().unwrap(),
                after.payload.bytes().unwrap()
            );
        }
    }

    #[test]
    fn maintenance_reads_every_codec_through_the_block_interface() {
        let points: Vec<_> = (-12..12)
            .map(|ts| Point::new(ts, ((ts * 3) % 7) as f64))
            .collect();
        for codec in Codec::ALL {
            let mut db = Db::with_block_size(8);
            db.write_many("s", &points).unwrap();
            for (block, chunk) in db
                .series
                .get_mut("s")
                .unwrap()
                .blocks
                .iter_mut()
                .zip(points.chunks(8))
            {
                block.codec = codec;
                block.payload = Payload::Memory(codec.encode(chunk).unwrap());
            }
            let buckets = db.downsample("s", -11, 10, 7, Agg::Count).unwrap();
            assert_eq!(
                buckets.points,
                vec![
                    Point::new(-14, 4.0),
                    Point::new(-7, 7.0),
                    Point::new(0, 7.0),
                    Point::new(7, 4.0),
                ],
                "{codec:?}"
            );
            db.delete_range("s", -9, 7).unwrap();
            assert_eq!(
                db.query(&Query::new("s", i64::MIN, i64::MAX))
                    .unwrap()
                    .points,
                [points[..3].to_vec(), points[20..].to_vec()].concat(),
                "{codec:?}"
            );
        }
    }
}
