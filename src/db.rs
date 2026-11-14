//! The database: series, blocks, queries, aggregates, and a single-file
//! on-disk format.
//!
//! Ingest is append-only per series. Points collect into a pending buffer and
//! get compressed into an immutable block once the buffer reaches
//! `block_size`. Queries never decompress a block that cannot contain a match:
//! block time bounds and value bounds are checked first, and full-block
//! aggregates (count, sum, avg, min, max) are answered from the header alone
//! when there is no value predicate.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::block::{Block, Payload, BLOCK_HEADER_BYTES};
use crate::codec::Codec;
use crate::crc::crc32;
use crate::wal::{self, Wal};
use crate::Point;

pub type Result<T> = std::result::Result<T, Error>;

const MAGIC_PREFIX: &[u8; 6] = b"SHRUNK";

/// Build the 8-byte magic for a format version: `SHRUNK` plus two decimal
/// digits. Keeping this derived means adding a format version only requires
/// changing `FORMAT_VERSION` (and, if needed, a legacy decoder).
const fn magic_for(version: u16) -> [u8; 8] {
    [
        MAGIC_PREFIX[0],
        MAGIC_PREFIX[1],
        MAGIC_PREFIX[2],
        MAGIC_PREFIX[3],
        MAGIC_PREFIX[4],
        MAGIC_PREFIX[5],
        b'0' + (version / 10) as u8,
        b'0' + (version % 10) as u8,
    ]
}

const VERSION_V1: u16 = 1;
const VERSION_V2: u16 = 2;
/// Current on-disk version. Change this one constant to bump the format; the
/// magic, writer, and new-file version all follow it.
///
/// * v1: original snapshot, CRC over payload only.
/// * v2: format header, CRC over block metadata plus payload.
/// * v3: separate header CRC and payload CRC, so open validates metadata
///   without reading payloads and query validates payloads lazily.
pub const FORMAT_VERSION: u16 = 3;
pub const FORMAT_MAGIC: [u8; 8] = magic_for(FORMAT_VERSION);
const MAGIC_V1: [u8; 8] = magic_for(VERSION_V1);
const MAGIC_V2: [u8; 8] = magic_for(VERSION_V2);
const MAX_SERIES: usize = 1_000_000;
const MAX_SERIES_NAME: usize = 1024;
const MAX_BLOCK_SIZE: usize = crate::block::MAX_BLOCK_POINTS as usize;
const MAX_DB_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_LEGACY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_BLOCKS: usize = 1_000_000;
const MAX_BATCH_POINTS: usize = 1_000_000;
const MAX_QUERY_POINTS: usize = 16_000_000;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    Io(std::io::Error),
    /// On-disk data that failed validation.
    Format(String),
    /// A caller-supplied argument or operation is not allowed.
    InvalidInput(String),
    /// A size, count, or allocation budget was exceeded.
    Limit(String),
    /// The database could not be opened because a writer holds the exclusive
    /// WAL lock, or because a concurrent checkpoint kept changing the files.
    Locked(String),
    UnknownSeries(String),
    OutOfOrder {
        series: String,
        last: i64,
        got: i64,
    },
    Duplicate {
        series: String,
        ts: i64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Format(m) => write!(f, "corrupt database: {m}"),
            Error::InvalidInput(m) | Error::Limit(m) | Error::Locked(m) => write!(f, "{m}"),
            Error::UnknownSeries(s) => write!(f, "unknown series: {s}"),
            Error::OutOfOrder { series, last, got } => write!(
                f,
                "out-of-order write to {series}: last timestamp {last}, got {got}"
            ),
            Error::Duplicate { series, ts } => write!(
                f,
                "duplicate timestamp {ts} in {series}; timestamps must strictly increase"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Predicate {
    Gt(f64),
    Ge(f64),
    Lt(f64),
    Le(f64),
    Eq(f64),
    Ne(f64),
}

impl Predicate {
    pub fn matches(self, v: f64) -> bool {
        match self {
            Predicate::Gt(x) => v > x,
            Predicate::Ge(x) => v >= x,
            Predicate::Lt(x) => v < x,
            Predicate::Le(x) => v <= x,
            Predicate::Eq(x) => v == x,
            Predicate::Ne(x) => v != x,
        }
    }

    /// Conservative check: `false` means no value in the block can match.
    fn may_match(self, b: &Block) -> bool {
        if b.has_nan {
            return true;
        }
        match self {
            Predicate::Gt(x) => b.v_max > x,
            Predicate::Ge(x) => b.v_max >= x,
            Predicate::Lt(x) => b.v_min < x,
            Predicate::Le(x) => b.v_min <= x,
            Predicate::Eq(x) => x >= b.v_min && x <= b.v_max,
            Predicate::Ne(x) => !(b.v_min == b.v_max && b.v_min == x),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agg {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl Agg {
    pub fn parse(s: &str) -> Option<Agg> {
        match s {
            "count" => Some(Agg::Count),
            "sum" => Some(Agg::Sum),
            "min" => Some(Agg::Min),
            "max" => Some(Agg::Max),
            "avg" | "mean" => Some(Agg::Avg),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct QueryStats {
    pub blocks_total: usize,
    pub blocks_visited: usize,
    pub blocks_decoded: usize,
    pub blocks_skipped_time: usize,
    pub blocks_skipped_value: usize,
    pub blocks_skipped_limit: usize,
    pub fast_blocks: usize,
    pub points_decoded: usize,
    pub pending_points: usize,
    pub bytes_decoded: usize,
}

impl QueryStats {
    pub fn compression_skips(&self) -> usize {
        self.blocks_skipped_time + self.blocks_skipped_value
    }
}

pub struct Query<'a> {
    pub series: &'a str,
    pub start: i64,
    pub end: i64,
    pub predicate: Option<Predicate>,
    pub limit: Option<usize>,
}

impl<'a> Query<'a> {
    pub fn new(series: &'a str, start: i64, end: i64) -> Self {
        Self {
            series,
            start,
            end,
            predicate: None,
            limit: None,
        }
    }

    pub fn filter(mut self, p: Predicate) -> Self {
        self.predicate = Some(p);
        self
    }

    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }
}

#[derive(Debug)]
pub struct QueryResult {
    pub points: Vec<Point>,
    pub stats: QueryStats,
}

#[derive(Debug)]
pub struct AggResult {
    pub value: Option<f64>,
    pub stats: QueryStats,
}

#[derive(Clone, Debug)]
pub struct CodecStat {
    pub codec: &'static str,
    pub blocks: usize,
    pub bytes: usize,
}

#[derive(Clone, Debug)]
pub struct SeriesSummary {
    pub name: String,
    pub points: u64,
    pub blocks: usize,
    pub payload_bytes: usize,
    pub stored_bytes: usize,
    pub raw_bytes: u64,
    pub codecs: Vec<CodecStat>,
}

impl SeriesSummary {
    pub fn ratio(&self) -> f64 {
        if self.stored_bytes == 0 {
            0.0
        } else {
            self.raw_bytes as f64 / self.stored_bytes as f64
        }
    }

    pub fn bits_per_point(&self) -> f64 {
        if self.points == 0 {
            0.0
        } else {
            self.stored_bytes as f64 * 8.0 / self.points as f64
        }
    }
}

#[derive(Debug)]
struct Series {
    name: String,
    blocks: Vec<Block>,
    pending: Vec<Point>,
    last_ts: Option<i64>,
}

impl Series {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            blocks: Vec::new(),
            pending: Vec::new(),
            last_ts: None,
        }
    }

    fn flush(&mut self, block_size: usize) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let should_merge = self
            .blocks
            .last()
            .is_some_and(|last| (last.count as usize) < block_size);
        if should_merge {
            // Decode first so a failure leaves the trailing block in place.
            let mut merged = self
                .blocks
                .last()
                .unwrap()
                .decode()
                .map_err(Error::Format)?;
            merged.append(&mut self.pending);
            self.blocks.pop();
            let mut rest = merged.as_slice();
            while rest.len() >= block_size {
                self.blocks.push(Block::build(&rest[..block_size]));
                rest = &rest[block_size..];
            }
            if !rest.is_empty() {
                self.blocks.push(Block::build(rest));
            }
            return Ok(());
        }
        self.blocks.push(Block::build(&self.pending));
        self.pending.clear();
        Ok(())
    }

    /// Extra blocks `flush` would add for the current pending buffer.
    fn flush_block_delta(&self, block_size: usize) -> usize {
        if self.pending.is_empty() {
            return 0;
        }
        if let Some(last) = self.blocks.last() {
            let last_count = last.count as usize;
            if last_count < block_size {
                let total = last_count + self.pending.len();
                // The short trailing block is replaced by one or more new blocks.
                return total.div_ceil(block_size).saturating_sub(1);
            }
        }
        1
    }

    fn point_count(&self) -> u64 {
        self.blocks.iter().map(|b| b.count as u64).sum::<u64>() + self.pending.len() as u64
    }
}

#[derive(Debug)]
pub struct Db {
    block_size: usize,
    format_version: u16,
    series: BTreeMap<String, Series>,
    path: Option<PathBuf>,
    wal: Option<Wal>,
}

impl Default for Db {
    fn default() -> Self {
        Self::new()
    }
}

impl Db {
    pub fn new() -> Self {
        Self::with_block_size(1024)
    }

    pub fn with_block_size(block_size: usize) -> Self {
        Self {
            block_size: block_size.clamp(1, MAX_BLOCK_SIZE),
            format_version: FORMAT_VERSION,
            series: BTreeMap::new(),
            path: None,
            wal: None,
        }
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// On-disk format version of the loaded file, or the version the next
    /// `save` will write for a new database.
    pub fn format_version(&self) -> u16 {
        self.format_version
    }

    pub fn series_names(&self) -> Vec<&str> {
        self.series.keys().map(|s| s.as_str()).collect()
    }

    pub fn point_count(&self) -> u64 {
        self.series.values().map(Series::point_count).sum()
    }

    /// Write one point. Timestamps must strictly increase per series; writing
    /// the same timestamp twice returns `Error::Duplicate`.
    pub fn write(&mut self, series: &str, ts: i64, value: f64) -> Result<()> {
        if self.wal.is_some() {
            return self.write_many(series, &[Point { ts, value }]);
        }
        validate_series_write(series, !self.series.contains_key(series), self.series.len())?;
        let block_size = self.block_size;
        let pending_len = self.series.get(series).map_or(0, |s| s.pending.len());
        self.ensure_block_budget(self.write_block_delta(series, pending_len, 1))?;
        let s = self
            .series
            .entry(series.to_string())
            .or_insert_with(|| Series::new(series));
        if let Some(last) = s.last_ts {
            if ts < last {
                return Err(Error::OutOfOrder {
                    series: series.to_string(),
                    last,
                    got: ts,
                });
            }
            if ts == last {
                return Err(Error::Duplicate {
                    series: series.to_string(),
                    ts,
                });
            }
        }
        s.last_ts = Some(ts);
        s.pending.push(Point { ts, value });
        if s.pending.len() >= block_size {
            let chunk = std::mem::take(&mut s.pending);
            s.blocks.push(Block::build(&chunk));
        }
        Ok(())
    }

    /// Write a batch atomically. The whole batch is validated before any point
    /// is accepted, so an out-of-order or duplicate point leaves the database
    /// unchanged. Timestamps must be strictly increasing, both within the
    /// batch and relative to the series' last timestamp.
    pub fn write_many(&mut self, series: &str, points: &[Point]) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        if points.len() > MAX_BATCH_POINTS {
            return Err(Error::Limit(format!(
                "batch exceeds {MAX_BATCH_POINTS} point limit"
            )));
        }

        validate_series_write(series, !self.series.contains_key(series), self.series.len())?;

        let existing_last = self.series.get(series).and_then(|s| s.last_ts);
        let mut prev = existing_last;
        for p in points {
            if let Some(last) = prev {
                if p.ts < last {
                    return Err(Error::OutOfOrder {
                        series: series.to_string(),
                        last,
                        got: p.ts,
                    });
                }
                if p.ts == last {
                    return Err(Error::Duplicate {
                        series: series.to_string(),
                        ts: p.ts,
                    });
                }
            }
            prev = Some(p.ts);
        }

        self.apply_points(series, points, true)
    }

    fn apply_points(&mut self, series: &str, points: &[Point], enforce_budget: bool) -> Result<()> {
        let pending_len = self.series.get(series).map_or(0, |s| s.pending.len());
        let block_size = self.block_size;
        if enforce_budget {
            self.ensure_block_budget(self.write_block_delta(series, pending_len, points.len()))?;
        }
        if let Some(wal) = self.wal.as_mut() {
            wal.append(series, points)?;
        }

        let s = self
            .series
            .entry(series.to_string())
            .or_insert_with(|| Series::new(series));
        s.last_ts = points.last().map(|p| p.ts);
        let mut remaining = points;
        if !s.pending.is_empty() {
            let room = block_size - s.pending.len();
            let take = room.min(remaining.len());
            if s.pending.len() + take == block_size {
                let mut chunk = std::mem::take(&mut s.pending);
                chunk.extend_from_slice(&remaining[..take]);
                s.blocks.push(Block::build(&chunk));
                remaining = &remaining[take..];
            } else {
                s.pending.extend_from_slice(remaining);
                remaining = &[];
            }
        }
        while remaining.len() >= block_size {
            s.blocks.push(Block::build(&remaining[..block_size]));
            remaining = &remaining[block_size..];
        }
        s.pending.extend_from_slice(remaining);
        Ok(())
    }

    /// Append a batch to a database opened for read/write. Returns an error if
    /// the database is not bound to a WAL path.
    pub fn append(&mut self, series: &str, points: &[Point]) -> Result<()> {
        if self.wal.is_none() {
            return Err(Error::InvalidInput(
                "append requires Db::open_rw or Db::open_rw_with_block_size".into(),
            ));
        }
        self.write_many(series, points)
    }

    pub fn flush(&mut self, series: &str) -> Result<()> {
        let block_size = self.block_size;
        let extra = self
            .series
            .get(series)
            .ok_or_else(|| Error::UnknownSeries(series.to_string()))?
            .flush_block_delta(block_size);
        self.ensure_block_budget(extra)?;
        self.series
            .get_mut(series)
            .ok_or_else(|| Error::UnknownSeries(series.to_string()))?
            .flush(block_size)
    }

    pub fn flush_all(&mut self) -> Result<()> {
        let extra = self
            .series
            .values()
            .map(|s| s.flush_block_delta(self.block_size))
            .sum();
        self.ensure_block_budget(extra)?;
        let block_size = self.block_size;
        for s in self.series.values_mut() {
            s.flush(block_size)?;
        }
        Ok(())
    }

    fn total_blocks(&self) -> usize {
        self.series.values().map(|s| s.blocks.len()).sum()
    }

    fn ensure_block_budget(&self, additional: usize) -> Result<()> {
        let total = self
            .total_blocks()
            .checked_add(additional)
            .ok_or_else(|| Error::Limit("block count overflow".into()))?;
        if total > MAX_TOTAL_BLOCKS {
            return Err(Error::Limit("snapshot block budget exceeded".into()));
        }
        Ok(())
    }

    /// Extra blocks this write would add, including the trailing block each
    /// series will need at flush, so an acknowledged write can be checkpointed.
    fn write_block_delta(&self, series: &str, pending_len: usize, new_points: usize) -> usize {
        let mut projected = 0usize;
        let mut seen = false;
        for (name, s) in &self.series {
            if name.as_str() == series {
                seen = true;
                projected = projected.saturating_add(projected_series_blocks(
                    self.block_size,
                    s.blocks.len(),
                    s.blocks.last().map(|b| b.count as usize).unwrap_or(0),
                    pending_len,
                    new_points,
                ));
            } else {
                projected = projected.saturating_add(
                    s.blocks
                        .len()
                        .saturating_add(s.flush_block_delta(self.block_size)),
                );
            }
        }
        if !seen {
            projected = projected.saturating_add(projected_series_blocks(
                self.block_size,
                0,
                0,
                0,
                new_points,
            ));
        }
        projected.saturating_sub(self.total_blocks())
    }

    /// Full scan. Time bounds are inclusive.
    pub fn query(&self, q: &Query) -> Result<QueryResult> {
        let s = self
            .series
            .get(q.series)
            .ok_or_else(|| Error::UnknownSeries(q.series.to_string()))?;

        let mut stats = QueryStats {
            blocks_total: s.blocks.len(),
            pending_points: s.pending.len(),
            ..Default::default()
        };
        let mut out = Vec::new();
        let limit = q.limit.unwrap_or(usize::MAX);

        if q.start <= q.end && limit > 0 {
            let first = s.blocks.partition_point(|b| b.t_max < q.start);
            stats.blocks_skipped_time += first;
            for (i, b) in s.blocks.iter().enumerate().skip(first) {
                if b.t_min > q.end {
                    stats.blocks_skipped_time += s.blocks.len() - i;
                    break;
                }
                if out.len() >= limit {
                    break;
                }
                stats.blocks_visited += 1;
                if let Some(pred) = q.predicate {
                    if !pred.may_match(b) {
                        stats.blocks_skipped_value += 1;
                        continue;
                    }
                }
                let points = b.decode().map_err(Error::Format)?;
                stats.blocks_decoded += 1;
                stats.points_decoded += points.len();
                stats.bytes_decoded += b.payload.len();
                for p in points {
                    if p.ts < q.start || p.ts > q.end {
                        continue;
                    }
                    if let Some(pred) = q.predicate {
                        if !pred.matches(p.value) {
                            continue;
                        }
                    }
                    if out.len() >= MAX_QUERY_POINTS {
                        return Err(Error::Limit(format!(
                            "query exceeds {MAX_QUERY_POINTS} point limit"
                        )));
                    }
                    if out.len() == out.capacity() {
                        out.try_reserve(1024)
                            .map_err(|_| Error::Limit("query allocation failed".into()))?;
                    }
                    out.push(p);
                    if out.len() >= limit {
                        break;
                    }
                }
            }

            if out.len() < limit {
                for p in &s.pending {
                    if p.ts < q.start {
                        continue;
                    }
                    if p.ts > q.end {
                        break;
                    }
                    if let Some(pred) = q.predicate {
                        if !pred.matches(p.value) {
                            continue;
                        }
                    }
                    if out.len() >= MAX_QUERY_POINTS {
                        return Err(Error::Limit(format!(
                            "query exceeds {MAX_QUERY_POINTS} point limit"
                        )));
                    }
                    if out.len() == out.capacity() {
                        out.try_reserve(1024)
                            .map_err(|_| Error::Limit("query allocation failed".into()))?;
                    }
                    out.push(*p);
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            stats.blocks_skipped_limit = stats
                .blocks_total
                .saturating_sub(stats.blocks_visited + stats.blocks_skipped_time);
        } else if limit == 0 {
            stats.blocks_skipped_limit = stats.blocks_total;
        } else {
            stats.blocks_skipped_time = stats.blocks_total;
        }

        Ok(QueryResult { points: out, stats })
    }

    /// Aggregate over an inclusive time range. Full blocks with no value
    /// predicate are answered from block metadata and never decoded.
    pub fn aggregate(
        &self,
        series: &str,
        start: i64,
        end: i64,
        agg: Agg,
        predicate: Option<Predicate>,
    ) -> Result<AggResult> {
        let s = self
            .series
            .get(series)
            .ok_or_else(|| Error::UnknownSeries(series.to_string()))?;

        let mut stats = QueryStats {
            blocks_total: s.blocks.len(),
            pending_points: s.pending.len(),
            ..Default::default()
        };
        let mut count: u64 = 0;
        let mut sum: f64 = 0.0;
        let mut extreme: Option<f64> = None;
        let mut matched = false;

        if start <= end {
            let first = s.blocks.partition_point(|b| b.t_max < start);
            for b in &s.blocks[first..] {
                if b.t_min > end {
                    break;
                }
                stats.blocks_visited += 1;

                if predicate.is_none() && b.t_min >= start && b.t_max <= end {
                    match agg {
                        Agg::Count => {
                            count += b.count as u64;
                            matched = true;
                            stats.fast_blocks += 1;
                            continue;
                        }
                        Agg::Sum | Agg::Avg if b.sum.is_some() => {
                            sum += b.sum.unwrap();
                            count += b.count as u64;
                            matched = true;
                            stats.fast_blocks += 1;
                            continue;
                        }
                        Agg::Min if !b.has_nan => {
                            extreme = Some(extreme.map_or(b.v_min, |e| e.min(b.v_min)));
                            matched = true;
                            stats.fast_blocks += 1;
                            continue;
                        }
                        Agg::Max if !b.has_nan => {
                            extreme = Some(extreme.map_or(b.v_max, |e| e.max(b.v_max)));
                            matched = true;
                            stats.fast_blocks += 1;
                            continue;
                        }
                        _ => {}
                    }
                }

                if let Some(pred) = predicate {
                    if !pred.may_match(b) {
                        stats.blocks_skipped_value += 1;
                        continue;
                    }
                }

                let points = b.decode().map_err(Error::Format)?;
                stats.blocks_decoded += 1;
                stats.points_decoded += points.len();
                stats.bytes_decoded += b.payload.len();
                for p in points {
                    if p.ts < start || p.ts > end {
                        continue;
                    }
                    if let Some(pred) = predicate {
                        if !pred.matches(p.value) {
                            continue;
                        }
                    }
                    fold(
                        agg,
                        p.value,
                        &mut count,
                        &mut sum,
                        &mut extreme,
                        &mut matched,
                    );
                }
            }

            for p in &s.pending {
                if p.ts < start {
                    continue;
                }
                if p.ts > end {
                    break;
                }
                if let Some(pred) = predicate {
                    if !pred.matches(p.value) {
                        continue;
                    }
                }
                fold(
                    agg,
                    p.value,
                    &mut count,
                    &mut sum,
                    &mut extreme,
                    &mut matched,
                );
            }
        }

        stats.blocks_skipped_time = stats.blocks_total.saturating_sub(stats.blocks_visited);
        let value = match agg {
            Agg::Count => {
                if matched {
                    Some(count as f64)
                } else {
                    None
                }
            }
            Agg::Sum => {
                if matched {
                    Some(sum)
                } else {
                    None
                }
            }
            Agg::Avg => {
                if count > 0 {
                    Some(sum / count as f64)
                } else {
                    None
                }
            }
            Agg::Min | Agg::Max => extreme,
        };
        Ok(AggResult { value, stats })
    }

    pub fn summaries(&self) -> Vec<SeriesSummary> {
        self.series
            .values()
            .map(|s| {
                let mut points = s.pending.len() as u64;
                let mut payload_bytes = 0usize;
                let mut stored_bytes = s.pending.len() * 16;
                let mut by_codec: BTreeMap<Codec, (usize, usize)> = BTreeMap::new();
                for b in &s.blocks {
                    points += b.count as u64;
                    payload_bytes += b.payload.len();
                    stored_bytes += b.stored_bytes();
                    let e = by_codec.entry(b.codec).or_insert((0, 0));
                    e.0 += 1;
                    e.1 += b.stored_bytes();
                }
                let codecs = by_codec
                    .into_iter()
                    .map(|(c, (blocks, bytes))| CodecStat {
                        codec: c.name(),
                        blocks,
                        bytes,
                    })
                    .collect();
                SeriesSummary {
                    name: s.name.clone(),
                    points,
                    blocks: s.blocks.len(),
                    payload_bytes,
                    stored_bytes,
                    raw_bytes: points * 16,
                    codecs,
                }
            })
            .collect()
    }

    /// Flush pending points and write a complete v3 snapshot atomically
    /// (temp file, fsync, rename, directory fsync). Legacy v1 files are
    /// upgraded in memory and become v3 after the first save.
    pub fn save(&mut self, path: &Path) -> Result<()> {
        self.save_with_hook(path, |_| Ok(()))
    }

    fn save_with_hook(
        &mut self,
        path: &Path,
        mut hook: impl FnMut(SaveStage) -> Result<()>,
    ) -> Result<()> {
        if self.wal.is_some() && !same_destination(self.path.as_deref().unwrap(), path)? {
            return Err(Error::InvalidInput(
                "cannot save a WAL-bound database to a different path".into(),
            ));
        }
        if self.wal.is_none() {
            match wal::wal_path(path).symlink_metadata() {
                Ok(_) => {
                    return Err(Error::InvalidInput(
                        "destination has a WAL; open it read/write and checkpoint instead".into(),
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        reject_symlink(path)?;
        self.flush_all()?;
        let file_name = path
            .file_name()
            .ok_or_else(|| Error::InvalidInput(format!("invalid path: {}", path.display())))?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let (tmp, mut f) = loop {
            let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let tmp = path.with_file_name(format!(
                ".{}.{}.{}.{}.tmp",
                file_name.to_string_lossy(),
                std::process::id(),
                nanos,
                seq
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&tmp) {
                Ok(f) => break (tmp, f),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        let write_result = (|| -> Result<()> {
            hook(SaveStage::Write)?;
            {
                let mut writer = BufWriter::with_capacity(64 * 1024, &mut f);
                write_db(self, &mut writer)?;
                writer.flush()?;
            }
            hook(SaveStage::FileSync)?;
            f.sync_all()?;
            hook(SaveStage::Rename)?;
            fs::rename(&tmp, path)?;
            hook(SaveStage::DirectorySync)?;
            // Opening and syncing a directory is a Unix operation. On other
            // platforms rename alone is the durability boundary.
            #[cfg(unix)]
            File::open(parent)?.sync_all()?;
            #[cfg(not(unix))]
            let _ = parent;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        write_result?;
        // The snapshot is now the durable source of truth. Truncating after
        // the rename is safe: a crash before this point replays a WAL whose
        // points are already in the snapshot, and replay skips them.
        if let Some(wal) = self.wal.as_mut() {
            hook(SaveStage::Truncate)?;
            wal.truncate()?;
        }
        self.path = Some(path.to_path_buf());
        self.format_version = FORMAT_VERSION;
        Ok(())
    }

    /// Flush pending points, write an atomic snapshot, and truncate the WAL.
    pub fn checkpoint(&mut self) -> Result<()> {
        let path = self.path.clone().ok_or_else(|| {
            Error::InvalidInput("checkpoint requires a bound database path".into())
        })?;
        self.save(&path)
    }

    /// Open a snapshot and replay any WAL. The returned handle is not bound
    /// for further appends; use [`Db::open_rw`] for that.
    ///
    /// The snapshot and WAL identities are re-checked after the read. If a
    /// concurrent writer checkpoints or appends mid-open, the open is retried
    /// so the handle is not assembled from a mixed generation.
    pub fn open(path: &Path) -> Result<Db> {
        reject_symlink(path)?;
        let wal_path = wal::wal_path(path);
        const OPEN_ATTEMPTS: u32 = 16;
        for _ in 0..OPEN_ATTEMPTS {
            let snap_id = file_identity(path)?;
            let wal_id = file_identity(&wal_path)?;
            let result = (|| -> Result<Db> {
                match (&snap_id, &wal_id) {
                    (None, None) => Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no snapshot or WAL at {}", path.display()),
                    )
                    .into()),
                    (None, Some(_)) => recover_from_wal(&wal_path),
                    (Some(_), _) => {
                        let mut db = open_snapshot(path)?;
                        replay_wal_into(&mut db, &wal_path)?;
                        Ok(db)
                    }
                }
            })();
            let snap_after = file_identity(path)?;
            let wal_after = file_identity(&wal_path)?;
            if open_generation_changed(snap_id, snap_after, wal_id, wal_after) {
                continue;
            }
            return result;
        }
        Err(Error::Locked(
            "database changed during open; retry later".into(),
        ))
    }

    /// Open for durable incremental appends. A missing database is created
    /// with the default block size; existing snapshots and WALs are replayed.
    pub fn open_rw(path: &Path) -> Result<Db> {
        Self::open_rw_with_block_size(path, 1024)
    }

    pub fn open_rw_with_block_size(path: &Path, block_size: usize) -> Result<Db> {
        reject_symlink(path)?;
        let wal_path = wal::wal_path(path);
        // Take the exclusive lock before reading the snapshot so a stale
        // writer cannot replay onto an old generation and overwrite a
        // newer checkpoint.
        let locked = Wal::lock_writer(&wal_path)?;
        let result = (|| -> Result<Db> {
            let has_snapshot = match path.symlink_metadata() {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(e.into()),
            };
            let mut db = if has_snapshot {
                open_snapshot(path)?
            } else {
                Db::with_block_size(block_size)
            };
            let expected = if has_snapshot {
                Some(db.block_size)
            } else if locked.len() >= wal::WAL_HEADER_BYTES {
                None
            } else {
                Some(db.block_size)
            };
            let (wal, wal_block_size) = locked.recover(expected, &mut |size, series, points| {
                if !has_snapshot {
                    db.block_size = size;
                }
                db.replay_record(series, points)
            })?;
            if !has_snapshot {
                db.block_size = wal_block_size;
            }
            db.wal = Some(wal);
            db.path = Some(path.to_path_buf());
            Ok(db)
        })();
        // Keep a newly created WAL on error. Once the lock is released,
        // unlinking its path could discard a second writer's acknowledged data.
        result
    }

    pub fn load_or_new(path: &Path) -> Result<Db> {
        if path.exists() || wal::wal_path(path).exists() {
            Db::open_rw(path)
        } else {
            Db::open_rw_with_block_size(path, 1024)
        }
    }

    fn replay_record(&mut self, series: &str, points: &[Point]) -> Result<()> {
        // partition_point and the query path both require ordered timestamps.
        // A valid checksum alone does not establish that semantic invariant.
        if points.windows(2).any(|pair| pair[0].ts >= pair[1].ts) {
            return Err(Error::Format(
                "WAL record timestamps are not strictly increasing".into(),
            ));
        }
        let last = self.series.get(series).and_then(|s| s.last_ts);
        let start = match last {
            Some(last) => points.partition_point(|p| p.ts <= last),
            None => 0,
        };
        if start < points.len() {
            validate_series_write(series, !self.series.contains_key(series), self.series.len())?;
            self.apply_points(series, &points[start..], false)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveStage {
    Write,
    FileSync,
    Rename,
    DirectorySync,
    Truncate,
}

fn open_snapshot(path: &Path) -> Result<Db> {
    let mut f = File::open(path)?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(Error::Format("snapshot is not a regular file".into()));
    }
    if meta.len() > MAX_DB_BYTES {
        return Err(Error::Format(format!(
            "database is {} bytes, limit is {MAX_DB_BYTES}",
            meta.len()
        )));
    }
    let mut magic = [0u8; 8];
    if f.read_exact(&mut magic).is_err() {
        return Err(Error::Format("truncated file header".into()));
    }
    match &magic {
        m if m == &FORMAT_MAGIC => decode_current_file(f, meta.len()),
        m if m == &MAGIC_V1 || m == &MAGIC_V2 => {
            if meta.len() > MAX_LEGACY_BYTES {
                return Err(Error::Format(format!(
                    "legacy snapshot exceeds {MAX_LEGACY_BYTES} byte read budget"
                )));
            }
            let mut data = Vec::new();
            data.try_reserve_exact(meta.len() as usize)
                .map_err(|_| Error::Limit("legacy snapshot allocation failed".into()))?;
            data.extend_from_slice(&magic);
            f.take(meta.len().saturating_sub(8))
                .read_to_end(&mut data)?;
            decode_db(&data)
        }
        _ => Err(Error::Format(format!(
            "bad magic; not a shrunk database (current format is SHRUNK{FORMAT_VERSION:02})"
        ))),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    len: u64,
    #[cfg(not(unix))]
    modified_nanos: Option<u128>,
}

impl FileIdentity {
    fn from_meta(meta: &std::fs::Metadata) -> Self {
        Self {
            #[cfg(unix)]
            dev: {
                use std::os::unix::fs::MetadataExt;
                meta.dev()
            },
            #[cfg(unix)]
            ino: {
                use std::os::unix::fs::MetadataExt;
                meta.ino()
            },
            len: meta.len(),
            #[cfg(not(unix))]
            modified_nanos: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos()),
        }
    }
}

fn file_identity(path: &Path) -> Result<Option<FileIdentity>> {
    match path.symlink_metadata() {
        Ok(meta) => Ok(Some(FileIdentity::from_meta(&meta))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Retry `open` only when the snapshot changed, or the WAL was replaced or
/// shrunk. Growth of an existing WAL is a consistent prefix and must not
/// starve readers under a live writer.
fn open_generation_changed(
    snap_before: Option<FileIdentity>,
    snap_after: Option<FileIdentity>,
    wal_before: Option<FileIdentity>,
    wal_after: Option<FileIdentity>,
) -> bool {
    if snap_before != snap_after {
        return true;
    }
    match (wal_before, wal_after) {
        (None, None) => false,
        (Some(before), Some(after)) => {
            if wal_replaced(before, after) {
                return true;
            }
            after.len < before.len
        }
        _ => true,
    }
}

fn wal_replaced(before: FileIdentity, after: FileIdentity) -> bool {
    #[cfg(unix)]
    {
        before.dev != after.dev || before.ino != after.ino
    }
    #[cfg(not(unix))]
    {
        let _ = (before, after);
        false
    }
}

fn projected_series_blocks(
    block_size: usize,
    existing_blocks: usize,
    last_count: usize,
    pending_len: usize,
    new_points: usize,
) -> usize {
    let last_short = existing_blocks > 0 && last_count < block_size;
    let points = pending_len + new_points + if last_short { last_count } else { 0 };
    let base = if last_short {
        existing_blocks - 1
    } else {
        existing_blocks
    };
    let extra = if points == 0 {
        0
    } else {
        points.div_ceil(block_size)
    };
    base.saturating_add(extra)
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(Error::InvalidInput(format!(
            "snapshot path {} is a symlink",
            path.display()
        ))),
        #[cfg(unix)]
        Ok(meta)
            if {
                use std::os::unix::fs::MetadataExt;
                meta.nlink() > 1
            } =>
        {
            Err(Error::InvalidInput(format!(
                "snapshot path {} has aliases",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn same_destination(a: &Path, b: &Path) -> Result<bool> {
    let key = |p: &Path| -> Result<PathBuf> {
        let name = p
            .file_name()
            .ok_or_else(|| Error::InvalidInput("path has no filename".into()))?;
        let parent = p
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Ok(fs::canonicalize(parent)?.join(name))
    };
    Ok(key(a)? == key(b)?)
}

fn replay_wal_into(db: &mut Db, wal_path: &Path) -> Result<()> {
    let len = match wal_path.symlink_metadata() {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if len < wal::WAL_HEADER_BYTES {
        return Ok(());
    }
    let block_size = wal::read_block_size(wal_path)?;
    if block_size != db.block_size {
        return Err(Error::Format(format!(
            "WAL block size {block_size} does not match database block size {}",
            db.block_size
        )));
    }
    wal::replay(wal_path, &mut |series, points| {
        db.replay_record(series, points)
    })
}

fn recover_from_wal(wal_path: &Path) -> Result<Db> {
    if wal_path.symlink_metadata()?.len() < wal::WAL_HEADER_BYTES {
        return Ok(Db::new());
    }
    let block_size = wal::read_block_size(wal_path)?;
    let mut db = Db::with_block_size(block_size);
    wal::replay(wal_path, &mut |series, points| {
        db.replay_record(series, points)
    })?;
    Ok(db)
}

fn fold(
    agg: Agg,
    v: f64,
    count: &mut u64,
    sum: &mut f64,
    extreme: &mut Option<f64>,
    matched: &mut bool,
) {
    *matched = true;
    match agg {
        Agg::Count => *count += 1,
        Agg::Sum => {
            if !v.is_nan() {
                *sum += v;
            }
        }
        Agg::Avg => {
            if !v.is_nan() {
                *sum += v;
                *count += 1;
            }
        }
        Agg::Min => {
            if !v.is_nan() {
                *extreme = Some(extreme.map_or(v, |e| e.min(v)));
            }
        }
        Agg::Max => {
            if !v.is_nan() {
                *extreme = Some(extreme.map_or(v, |e| e.max(v)));
            }
        }
    }
}

#[cfg(test)]
fn encode_db(db: &Db) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_db(db, &mut out)?;
    Ok(out)
}

fn write_db(db: &Db, out: &mut impl Write) -> Result<()> {
    if db.series.len() > MAX_SERIES {
        return Err(Error::Limit(format!(
            "series count {} exceeds limit {MAX_SERIES}",
            db.series.len()
        )));
    }
    for s in db.series.values() {
        if s.name.len() > MAX_SERIES_NAME {
            return Err(Error::InvalidInput(format!(
                "series name length {} exceeds limit {MAX_SERIES_NAME}",
                s.name.len()
            )));
        }
    }
    let mut total_blocks = 0usize;
    for s in db.series.values() {
        total_blocks = total_blocks
            .checked_add(s.blocks.len())
            .ok_or_else(|| Error::Limit("block count overflow".into()))?;
    }
    if total_blocks > MAX_TOTAL_BLOCKS {
        return Err(Error::Limit("snapshot block budget exceeded".into()));
    }
    out.write_all(&FORMAT_MAGIC)?;
    out.write_all(&FORMAT_VERSION.to_le_bytes())?;
    out.write_all(&0u16.to_le_bytes())?;
    out.write_all(&(db.block_size as u32).to_le_bytes())?;
    out.write_all(&(db.series.len() as u32).to_le_bytes())?;

    for s in db.series.values() {
        let name = s.name.as_bytes();
        out.write_all(&(name.len() as u32).to_le_bytes())?;
        out.write_all(name)?;
        out.write_all(&(s.blocks.len() as u32).to_le_bytes())?;

        for b in &s.blocks {
            b.validate_meta().map_err(Error::Format)?;
            b.payload
                .with_bytes(|payload| -> Result<()> {
                    if let Some(expected) = b.payload_crc {
                        if crc32(payload) != expected {
                            return Err(Error::Format(format!(
                                "series {:?}: payload changed before save",
                                s.name
                            )));
                        }
                    }
                    let mut header = Vec::with_capacity(50);
                    header.push(b.codec as u8);
                    let mut flags = 0u8;
                    if b.sum.is_some() {
                        flags |= 1;
                    }
                    if b.has_nan {
                        flags |= 2;
                    }
                    header.push(flags);
                    header.extend_from_slice(&b.count.to_le_bytes());
                    header.extend_from_slice(&b.t_min.to_le_bytes());
                    header.extend_from_slice(&b.t_max.to_le_bytes());
                    header.extend_from_slice(&b.v_min.to_le_bytes());
                    header.extend_from_slice(&b.v_max.to_le_bytes());
                    header.extend_from_slice(&b.sum.unwrap_or(f64::NAN).to_le_bytes());
                    header.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                    // Header CRC lets `open` validate query-planning metadata without
                    // reading any payload bytes.
                    let header_crc = crc32(&header);
                    out.write_all(&header)?;
                    out.write_all(&header_crc.to_le_bytes())?;
                    out.write_all(payload)?;
                    out.write_all(&crc32(payload).to_le_bytes())?;
                    Ok(())
                })
                .map_err(Error::Format)??;
        }
    }
    Ok(())
}

fn validate_series_write(series: &str, is_new: bool, current_count: usize) -> Result<()> {
    if series.len() > MAX_SERIES_NAME {
        return Err(Error::InvalidInput(format!(
            "series name length {} exceeds limit {MAX_SERIES_NAME}",
            series.len()
        )));
    }
    if is_new && current_count >= MAX_SERIES {
        return Err(Error::Limit(format!(
            "series count limit {MAX_SERIES} reached"
        )));
    }
    Ok(())
}

fn decode_db(data: &[u8]) -> Result<Db> {
    if data.len() < 8 {
        return Err(Error::Format("truncated file header".into()));
    }
    match &data[..8] {
        m if m == MAGIC_V1 => decode_v1(data),
        m if m == MAGIC_V2 => decode_v2(data),
        _ => Err(Error::Format(format!(
            "bad magic; not a shrunk database (current format is SHRUNK{FORMAT_VERSION:02})"
        ))),
    }
}

fn decode_v1(data: &[u8]) -> Result<Db> {
    let mut c = Cursor::new(data);
    c.take(8)?;
    let block_size = c.u32()? as usize;
    let series_count = c.u32()? as usize;
    let mut db = decode_body(&mut c, data, block_size, series_count, false)?;
    reverify_v1_metadata(&mut db)?;
    db.format_version = VERSION_V1;
    Ok(db)
}

// v1 checksums cover only payloads. Recompute planning metadata from those
// payloads before queries can trust it.
fn reverify_v1_metadata(db: &mut Db) -> Result<()> {
    for (name, series) in &mut db.series {
        for block in &mut series.blocks {
            let points = block
                .decode()
                .map_err(|e| Error::Format(format!("series {name:?}: invalid v1 payload: {e}")))?;
            let first = points
                .first()
                .ok_or_else(|| Error::Format("empty v1 block".into()))?;
            let mut v_min = f64::INFINITY;
            let mut v_max = f64::NEG_INFINITY;
            let mut has_nan = false;
            let mut finite = true;
            let mut sum = 0.0;
            for p in &points {
                if p.value.is_nan() {
                    has_nan = true;
                    finite = false;
                } else {
                    v_min = v_min.min(p.value);
                    v_max = v_max.max(p.value);
                    if p.value.is_finite() {
                        sum += p.value;
                    } else {
                        finite = false;
                    }
                }
            }
            block.t_min = first.ts;
            block.t_max = points.last().unwrap().ts;
            block.v_min = v_min;
            block.v_max = v_max;
            block.has_nan = has_nan;
            block.sum = if finite && sum.is_finite() {
                Some(sum)
            } else {
                None
            };
            block.validate_meta().map_err(Error::Format)?;
        }
        let mut prev = None;
        for block in &series.blocks {
            if prev.is_some_and(|end| block.t_min <= end) {
                return Err(Error::Format(format!(
                    "series {name:?}: overlapping v1 blocks"
                )));
            }
            prev = Some(block.t_max);
        }
        series.last_ts = prev;
    }
    Ok(())
}

fn decode_v2(data: &[u8]) -> Result<Db> {
    let mut c = Cursor::new(data);
    c.take(8)?;
    let version = u16::from_le_bytes(c.take(2)?.try_into().unwrap());
    let flags = u16::from_le_bytes(c.take(2)?.try_into().unwrap());
    if version != VERSION_V2 {
        return Err(Error::Format(format!(
            "unsupported v2 format version {version}; expected 2"
        )));
    }
    if flags != 0 {
        return Err(Error::Format(format!(
            "unsupported format flags {flags:#x}"
        )));
    }
    let block_size = c.u32()? as usize;
    let series_count = c.u32()? as usize;
    let mut db = decode_body(&mut c, data, block_size, series_count, true)?;
    db.format_version = VERSION_V2;
    Ok(db)
}

/// Parse a v3 file lazily. Only block headers and series metadata are read;
/// payloads stay on disk until a query decodes the block.
fn decode_current_file(file: File, len: u64) -> Result<Db> {
    let file = Arc::new(file);
    let mut c = FileCursor::new(file, len, 8);
    let version = c.u16()?;
    let flags = c.u16()?;
    if version != FORMAT_VERSION {
        return Err(Error::Format(format!(
            "unsupported format version {version}; this build reads version {FORMAT_VERSION}"
        )));
    }
    if flags != 0 {
        return Err(Error::Format(format!(
            "unsupported format flags {flags:#x}"
        )));
    }
    let block_size = c.u32()? as usize;
    let series_count = c.u32()? as usize;
    if !(1..=MAX_BLOCK_SIZE).contains(&block_size) {
        return Err(Error::Format(format!(
            "block size {block_size} outside 1..={MAX_BLOCK_SIZE}"
        )));
    }
    if series_count > MAX_SERIES {
        return Err(Error::Format(format!(
            "series count {series_count} exceeds limit {MAX_SERIES}"
        )));
    }
    if series_count as u64 > c.remaining() / 8 {
        return Err(Error::Format("series count exceeds file size".into()));
    }

    let mut db = Db::with_block_size(block_size);
    let mut total_blocks = 0usize;
    for _ in 0..series_count {
        let name_len = c.u32()? as usize;
        if name_len > MAX_SERIES_NAME {
            return Err(Error::Format(format!(
                "series name length {name_len} exceeds limit {MAX_SERIES_NAME}"
            )));
        }
        let name = String::from_utf8(c.take(name_len)?)
            .map_err(|_| Error::Format("series name is not utf-8".into()))?;
        let block_count = c.u32()? as usize;
        total_blocks = total_blocks
            .checked_add(block_count)
            .ok_or_else(|| Error::Limit("block count overflow".into()))?;
        if total_blocks > MAX_TOTAL_BLOCKS {
            return Err(Error::Limit("snapshot block budget exceeded".into()));
        }
        if block_count as u64 > c.remaining() / BLOCK_HEADER_BYTES as u64 {
            return Err(Error::Format(format!(
                "series {name:?}: block count {block_count} exceeds remaining file size"
            )));
        }
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(block_count)
            .map_err(|_| Error::Limit("block index allocation failed".into()))?;
        let mut prev_t_max: Option<i64> = None;
        for _ in 0..block_count {
            let block = read_current_block(&mut c, &name)?;
            if let Some(prev) = prev_t_max {
                if block.t_min <= prev {
                    return Err(Error::Format(format!(
                        "series {name:?}: block starting at {} overlaps previous block ending at {prev}",
                        block.t_min
                    )));
                }
            }
            prev_t_max = Some(block.t_max);
            blocks.push(block);
        }
        let last_ts = blocks.last().map(|b| b.t_max);
        if db
            .series
            .insert(
                name.clone(),
                Series {
                    name,
                    blocks,
                    pending: Vec::new(),
                    last_ts,
                },
            )
            .is_some()
        {
            return Err(Error::Format("duplicate series name".into()));
        }
    }
    if c.pos != len {
        return Err(Error::Format(format!(
            "{} trailing bytes after database",
            len - c.pos
        )));
    }
    db.format_version = FORMAT_VERSION;
    Ok(db)
}

fn read_current_block(c: &mut FileCursor, name: &str) -> Result<Block> {
    let header = c.take(BLOCK_HEADER_BYTES)?;
    let header_crc = u32::from_le_bytes(header[50..54].try_into().unwrap());
    let actual = crc32(&header[..50]);
    if actual != header_crc {
        return Err(Error::Format(format!(
            "series {name:?}: block header checksum mismatch"
        )));
    }
    let codec = Codec::from_u8(header[0])
        .ok_or_else(|| Error::Format(format!("series {name:?}: unknown codec id")))?;
    let flags = header[1];
    if flags & !3 != 0 {
        return Err(Error::Format(format!(
            "series {name:?}: unknown block flags {flags:#x}"
        )));
    }
    let count = u32::from_le_bytes(header[2..6].try_into().unwrap());
    let t_min = i64::from_le_bytes(header[6..14].try_into().unwrap());
    let t_max = i64::from_le_bytes(header[14..22].try_into().unwrap());
    let v_min = f64::from_le_bytes(header[22..30].try_into().unwrap());
    let v_max = f64::from_le_bytes(header[30..38].try_into().unwrap());
    let raw_sum = f64::from_le_bytes(header[38..46].try_into().unwrap());
    let payload_len = u32::from_le_bytes(header[46..50].try_into().unwrap()) as usize;
    if payload_len > crate::block::MAX_PAYLOAD_BYTES {
        return Err(Error::Format(format!(
            "series {name:?}: payload length {payload_len} exceeds limit {}",
            crate::block::MAX_PAYLOAD_BYTES
        )));
    }
    let offset = c.pos;
    c.skip(payload_len as u64)?;
    let payload_crc = c.u32()?;
    let block = Block {
        codec,
        count,
        t_min,
        t_max,
        v_min,
        v_max,
        has_nan: flags & 2 != 0,
        sum: if flags & 1 != 0 { Some(raw_sum) } else { None },
        payload: Payload::File {
            file: c.file.clone(),
            offset,
            len: payload_len as u32,
        },
        payload_crc: Some(payload_crc),
    };
    block
        .validate_meta()
        .map_err(|e| Error::Format(format!("series {name:?}: {e}")))?;
    Ok(block)
}

fn decode_body(
    c: &mut Cursor<'_>,
    data: &[u8],
    block_size: usize,
    series_count: usize,
    v2_crc: bool,
) -> Result<Db> {
    if !(1..=MAX_BLOCK_SIZE).contains(&block_size) {
        return Err(Error::Format(format!(
            "block size {block_size} outside 1..={MAX_BLOCK_SIZE}"
        )));
    }
    if series_count > MAX_SERIES {
        return Err(Error::Format(format!(
            "series count {series_count} exceeds limit {MAX_SERIES}"
        )));
    }
    if series_count > data.len().saturating_sub(c.pos) / 8 {
        return Err(Error::Format("series count exceeds file size".into()));
    }

    let mut db = Db::with_block_size(block_size);
    let mut total_blocks = 0usize;
    for _ in 0..series_count {
        let name_len = c.u32()? as usize;
        if name_len > MAX_SERIES_NAME {
            return Err(Error::Format(format!(
                "series name length {name_len} exceeds limit {MAX_SERIES_NAME}"
            )));
        }
        let name = String::from_utf8(c.take(name_len)?.to_vec())
            .map_err(|_| Error::Format("series name is not utf-8".into()))?;
        let block_count = c.u32()? as usize;
        total_blocks = total_blocks
            .checked_add(block_count)
            .ok_or_else(|| Error::Limit("block count overflow".into()))?;
        if total_blocks > MAX_TOTAL_BLOCKS {
            return Err(Error::Limit("snapshot block budget exceeded".into()));
        }
        if block_count > (data.len().saturating_sub(c.pos)) / BLOCK_HEADER_BYTES {
            return Err(Error::Format(format!(
                "series {name:?}: block count {block_count} exceeds remaining file size"
            )));
        }
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(block_count)
            .map_err(|_| Error::Limit("block index allocation failed".into()))?;
        let mut prev_t_max: Option<i64> = None;

        for _ in 0..block_count {
            let block = read_block(c, data, &name, v2_crc)?;
            if let Some(prev) = prev_t_max {
                if block.t_min <= prev {
                    return Err(Error::Format(format!(
                        "series {name:?}: block starting at {} overlaps previous block ending at {prev}",
                        block.t_min
                    )));
                }
            }
            prev_t_max = Some(block.t_max);
            blocks.push(block);
        }

        let last_ts = blocks.last().map(|b| b.t_max);
        if db
            .series
            .insert(
                name.clone(),
                Series {
                    name,
                    blocks,
                    pending: Vec::new(),
                    last_ts,
                },
            )
            .is_some()
        {
            return Err(Error::Format("duplicate series name".into()));
        }
    }

    if c.pos != data.len() {
        return Err(Error::Format(format!(
            "{} trailing bytes after database",
            data.len() - c.pos
        )));
    }
    Ok(db)
}

fn read_block(c: &mut Cursor<'_>, data: &[u8], name: &str, v2_crc: bool) -> Result<Block> {
    let start = c.pos;
    let codec = Codec::from_u8(c.u8()?)
        .ok_or_else(|| Error::Format(format!("series {name:?}: unknown codec id")))?;
    let flags = c.u8()?;
    if flags & !3 != 0 {
        return Err(Error::Format(format!(
            "series {name:?}: unknown block flags {flags:#x}"
        )));
    }
    let count = c.u32()?;
    let t_min = c.i64()?;
    let t_max = c.i64()?;
    let v_min = c.f64()?;
    let v_max = c.f64()?;
    let raw_sum = c.f64()?;
    let payload_len = c.u32()? as usize;
    if payload_len > crate::block::MAX_PAYLOAD_BYTES {
        return Err(Error::Format(format!(
            "series {name:?}: payload length {payload_len} exceeds limit {}",
            crate::block::MAX_PAYLOAD_BYTES
        )));
    }
    let payload = c.take(payload_len)?.to_vec();
    let crc = c.u32()?;
    let calculated = if v2_crc {
        // `c.pos` is just past the CRC, so the covered region ends 4 bytes
        // before it and starts at the block's first header byte.
        crc32(&data[start..c.pos - 4])
    } else {
        crc32(&payload)
    };
    if calculated != crc {
        return Err(Error::Format(format!(
            "checksum mismatch in series {name:?}"
        )));
    }
    let block = Block {
        codec,
        count,
        t_min,
        t_max,
        v_min,
        v_max,
        has_nan: flags & 2 != 0,
        sum: if flags & 1 != 0 { Some(raw_sum) } else { None },
        payload: Payload::Memory(payload),
        payload_crc: None,
    };
    block
        .validate_meta()
        .map_err(|e| Error::Format(format!("series {name:?}: {e}")))?;
    Ok(block)
}

const FILE_CURSOR_BUF: usize = 64 * 1024;

struct FileCursor {
    file: Arc<File>,
    pos: u64,
    len: u64,
    buf: Vec<u8>,
    buf_off: u64,
}

impl FileCursor {
    fn new(file: Arc<File>, len: u64, pos: u64) -> Self {
        Self {
            file,
            pos,
            len,
            buf: Vec::new(),
            buf_off: 0,
        }
    }

    fn remaining(&self) -> u64 {
        self.len.saturating_sub(self.pos)
    }

    fn fill(&mut self) -> Result<()> {
        let start = self.pos;
        let n = (self.len - start).min(FILE_CURSOR_BUF as u64) as usize;
        self.buf.clear();
        self.buf
            .try_reserve_exact(n)
            .map_err(|_| Error::Limit("file read allocation failed".into()))?;
        self.buf.resize(n, 0);
        crate::block::read_at(&self.file, start, &mut self.buf).map_err(Error::Format)?;
        self.buf_off = start;
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<Vec<u8>> {
        let want = n as u64;
        if want > self.remaining() {
            return Err(Error::Format(format!(
                "truncated at byte {} (wanted {want} more)",
                self.pos
            )));
        }
        let mut out = Vec::new();
        out.try_reserve_exact(n)
            .map_err(|_| Error::Limit("file read allocation failed".into()))?;
        out.resize(n, 0);
        let mut filled = 0;
        while filled < n {
            let in_buf =
                self.pos >= self.buf_off && self.pos < self.buf_off + self.buf.len() as u64;
            if !in_buf {
                if n - filled >= FILE_CURSOR_BUF {
                    crate::block::read_at(&self.file, self.pos, &mut out[filled..])
                        .map_err(Error::Format)?;
                    self.pos += (n - filled) as u64;
                    self.buf.clear();
                    return Ok(out);
                }
                self.fill()?;
            }
            let buf_pos = (self.pos - self.buf_off) as usize;
            let avail = self.buf.len().saturating_sub(buf_pos);
            let take = avail.min(n - filled);
            out[filled..filled + take].copy_from_slice(&self.buf[buf_pos..buf_pos + take]);
            self.pos += take as u64;
            filled += take;
        }
        Ok(out)
    }

    fn skip(&mut self, n: u64) -> Result<()> {
        if n > self.remaining() {
            return Err(Error::Format(format!(
                "truncated at byte {} (wanted to skip {n})",
                self.pos
            )));
        }
        self.pos += n;
        Ok(())
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| Error::Format("size overflow".into()))?;
        if end > self.data.len() {
            return Err(Error::Format(format!(
                "truncated at byte {} (wanted {n} more)",
                self.pos
            )));
        }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_order_rejected() {
        let mut db = Db::new();
        db.write("s", 10, 1.0).unwrap();
        let err = db.write("s", 9, 2.0).unwrap_err();
        assert!(matches!(err, Error::OutOfOrder { .. }));
    }

    #[test]
    fn unknown_series() {
        let db = Db::new();
        assert!(matches!(
            db.query(&Query::new("nope", 0, 10)),
            Err(Error::UnknownSeries(_))
        ));
    }

    #[test]
    fn summaries_count_pending() {
        let mut db = Db::with_block_size(100);
        for i in 0..10 {
            db.write("s", i, i as f64).unwrap();
        }
        let s = db.summaries();
        assert_eq!(s[0].points, 10);
        assert_eq!(s[0].blocks, 0);
        db.flush("s").unwrap();
        let s = db.summaries();
        assert_eq!(s[0].blocks, 1);
    }

    #[test]
    fn write_rejects_overlong_series_name_without_mutation() {
        let mut db = Db::new();
        let name = "x".repeat(MAX_SERIES_NAME + 1);
        assert!(matches!(
            db.write(&name, 1, 1.0),
            Err(Error::InvalidInput(_))
        ));
        assert!(db.series.is_empty());
    }

    #[test]
    fn series_limit_applies_to_new_names_only() {
        assert!(validate_series_write("new", true, MAX_SERIES).is_err());
        assert!(validate_series_write("existing", false, MAX_SERIES).is_ok());
    }

    #[test]
    fn encode_rejects_overlong_series_name() {
        let mut db = Db::new();
        let name = "x".repeat(MAX_SERIES_NAME + 1);
        db.series.insert(name.clone(), Series::new(&name));
        assert!(matches!(encode_db(&db), Err(Error::InvalidInput(_))));
    }

    #[test]
    fn wal_bound_database_cannot_save_to_another_path() {
        let base = std::env::temp_dir().join(format!("shrunk-save-path-{}", std::process::id()));
        let path = base.with_extension("db");
        let other = base.with_extension("other.db");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&other);
        let _ = std::fs::remove_file(wal::wal_path(&path));

        let mut db = Db::open_rw(&path).unwrap();
        assert!(matches!(db.save(&other), Err(Error::InvalidInput(_))));
        assert!(!other.exists());
        db.write("s", 1, 2.0).unwrap();
        db.save(&path).unwrap();

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(wal::wal_path(&path));
    }

    #[test]
    fn snapshot_failure_stages_never_truncate_wal_early() {
        for (number, stage) in [
            SaveStage::Write,
            SaveStage::FileSync,
            SaveStage::Rename,
            SaveStage::DirectorySync,
            SaveStage::Truncate,
        ]
        .into_iter()
        .enumerate()
        {
            let path = std::env::temp_dir()
                .join(format!("shrunk-stage-{}-{number}.db", std::process::id()));
            let wal_path = wal::wal_path(&path);
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(&wal_path);
            let mut db = Db::open_rw(&path).unwrap();
            db.append("s", &[Point { ts: 1, value: 4.0 }]).unwrap();
            let wal_len = fs::metadata(&wal_path).unwrap().len();
            let error = db.save_with_hook(&path, |at| {
                if at == stage {
                    Err(Error::Io(std::io::Error::other("injected failure")))
                } else {
                    Ok(())
                }
            });
            assert!(error.is_err(), "stage {stage:?}");
            assert_eq!(
                fs::metadata(&wal_path).unwrap().len(),
                wal_len,
                "stage {stage:?}"
            );
            drop(db);
            let scan = Db::open(&path)
                .unwrap()
                .query(&Query::new("s", 0, 2))
                .unwrap();
            assert_eq!(scan.points.len(), 1, "stage {stage:?}");
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(&wal_path);
        }
    }

    #[test]
    fn unbound_save_rejects_destination_with_wal_only_data() {
        let path = std::env::temp_dir().join(format!("shrunk-conflict-{}.db", std::process::id()));
        let wal_path = wal::wal_path(&path);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&wal_path);
        let mut writer = Db::open_rw(&path).unwrap();
        writer
            .append("old", &[Point { ts: 1, value: 7.0 }])
            .unwrap();
        drop(writer);
        let before = fs::read(&wal_path).unwrap();
        let mut fresh = Db::new();
        fresh.write("new", 2, 8.0).unwrap();
        assert!(matches!(fresh.save(&path), Err(Error::InvalidInput(_))));
        assert!(!path.exists());
        assert_eq!(fs::read(&wal_path).unwrap(), before);
        let _ = fs::remove_file(&wal_path);
    }

    #[test]
    fn v1_sum_is_derived_from_payload_not_mutable_header() {
        let mut data = include_bytes!("../tests/fixtures/v1.shrunk").to_vec();
        let first_block = 8 + 4 + 4 + 4 + 7 + 4;
        data[first_block + 38..first_block + 46].copy_from_slice(&9999.0f64.to_le_bytes());
        let db = decode_v1(&data).unwrap();
        let points = db
            .query(&Query::new("fixture", i64::MIN, i64::MAX))
            .unwrap()
            .points;
        let expected: f64 = points
            .iter()
            .filter(|p| p.value.is_finite())
            .map(|p| p.value)
            .sum();
        let actual = db
            .aggregate("fixture", i64::MIN, i64::MAX, Agg::Sum, None)
            .unwrap()
            .value
            .unwrap();
        assert!((actual - expected).abs() < 1e-9);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_aliases_are_rejected_for_writers() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("shrunk-alias-{}", std::process::id()));
        let path = base.with_extension("db");
        let alias = base.with_extension("alias");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&alias);
        let mut db = Db::new();
        db.write("s", 1, 1.0).unwrap();
        db.save(&path).unwrap();
        symlink(&path, &alias).unwrap();
        assert!(Db::open_rw(&alias).is_err());
        fs::remove_file(&alias).unwrap();
        fs::hard_link(&path, &alias).unwrap();
        assert!(Db::open_rw(&alias).is_err());
        fs::remove_file(&alias).unwrap();
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn partial_wal_header_does_not_block_open() {
        let path =
            std::env::temp_dir().join(format!("shrunk-partial-wal-{}.db", std::process::id()));
        let wal_path = wal::wal_path(&path);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&wal_path);
        let mut db = Db::new();
        db.write("s", 1, 1.0).unwrap();
        db.save(&path).unwrap();
        fs::write(&wal_path, b"SHRUNKWL").unwrap();

        let read = Db::open(&path).unwrap();
        assert_eq!(read.point_count(), 1);

        let mut rw = Db::open_rw(&path).unwrap();
        rw.append("s", &[Point { ts: 2, value: 2.0 }]).unwrap();
        drop(rw);
        let read = Db::open(&path).unwrap();
        assert_eq!(read.point_count(), 2);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&wal_path);
    }

    #[test]
    fn torn_wal_length_at_eof_is_truncated_on_rw_open() {
        let path =
            std::env::temp_dir().join(format!("shrunk-bad-length-{}.db", std::process::id()));
        let wal_path = wal::wal_path(&path);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&wal_path);
        let mut db = Db::open_rw(&path).unwrap();
        db.append("s", &[Point { ts: 1, value: 1.0 }]).unwrap();
        drop(db);
        let mut bytes = fs::read(&wal_path).unwrap();
        bytes[16..20].copy_from_slice(&1000u32.to_le_bytes());
        fs::write(&wal_path, &bytes).unwrap();
        // Read-only open ignores the torn record and leaves the file alone.
        // The only record is itself torn, so nothing is replayed.
        assert_eq!(Db::open(&path).unwrap().point_count(), 0);
        assert_eq!(fs::read(&wal_path).unwrap(), bytes);
        let rw = Db::open_rw(&path).unwrap();
        assert_eq!(rw.point_count(), 0);
        // Writer truncates back to a well-formed header.
        assert!(fs::read(&wal_path).unwrap().len() < bytes.len());
        drop(rw);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&wal_path);
    }

    #[test]
    fn replay_rejects_unordered_records_even_when_snapshot_would_skip_them() {
        let mut db = Db::new();
        db.write("s", 5, 1.0).unwrap();
        let points = [Point { ts: 7, value: 1.0 }, Point { ts: 4, value: 2.0 }];
        assert!(matches!(
            db.replay_record("s", &points),
            Err(Error::Format(_))
        ));
        assert_eq!(db.point_count(), 1);
    }

    #[test]
    fn failed_writer_open_keeps_lock_file_for_retry() {
        let path =
            std::env::temp_dir().join(format!("shrunk-failed-open-{}.db", std::process::id()));
        let wal_path = wal::wal_path(&path);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&wal_path);
        fs::write(&path, b"bad snapshot").unwrap();
        assert!(Db::open_rw(&path).is_err());
        assert!(wal_path.exists());
        fs::remove_file(&path).unwrap();
        let mut db = Db::open_rw(&path).unwrap();
        db.write("s", 1, 2.0).unwrap();
        drop(db);
        assert_eq!(Db::open(&path).unwrap().point_count(), 1);
        let _ = fs::remove_file(&wal_path);
    }

    #[test]
    fn second_writer_is_rejected_while_lock_is_held() {
        let path =
            std::env::temp_dir().join(format!("shrunk-lock-order-{}.db", std::process::id()));
        let wal_path = wal::wal_path(&path);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&wal_path);
        let mut db = Db::open_rw(&path).unwrap();
        db.append("s", &[Point { ts: 1, value: 1.0 }]).unwrap();
        let err = Db::open_rw(&path).unwrap_err();
        assert!(
            matches!(err, Error::Locked(_)) || err.to_string().contains("locked"),
            "{err}"
        );
        drop(db);
        assert!(Db::open_rw(&path).is_ok());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&wal_path);
    }
}
