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
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use crate::block::{Block, BLOCK_HEADER_BYTES};
use crate::codec::Codec;
use crate::Point;

pub type Result<T> = std::result::Result<T, Error>;

const MAGIC_V1: &[u8; 8] = b"SHRUNK01";
const MAGIC_V2: &[u8; 8] = b"SHRUNK02";
/// Current on-disk version. Version 1 used payload-only CRCs and no explicit
/// version field; version 2 adds a format header and CRC coverage over block
/// metadata as well as the payload.
pub const FORMAT_VERSION: u16 = 2;
const MAX_SERIES: usize = 1_000_000;
const MAX_SERIES_NAME: usize = 1024;
const MAX_BLOCK_SIZE: usize = crate::block::MAX_BLOCK_POINTS as usize;
const MAX_DB_BYTES: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Format(String),
    UnknownSeries(String),
    OutOfOrder { series: String, last: i64, got: i64 },
    Duplicate { series: String, ts: i64 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Format(m) => write!(f, "corrupt database: {m}"),
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

    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        self.blocks.push(Block::build(&self.pending));
        self.pending.clear();
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
        self.write_many(series, &[Point { ts, value }])
    }

    /// Write a batch atomically. The whole batch is validated before any point
    /// is accepted, so an out-of-order or duplicate point leaves the database
    /// unchanged. Timestamps must be strictly increasing, both within the
    /// batch and relative to the series' last timestamp.
    pub fn write_many(&mut self, series: &str, points: &[Point]) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }

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

        let block_size = self.block_size;
        let s = self
            .series
            .entry(series.to_string())
            .or_insert_with(|| Series::new(series));
        s.last_ts = prev;
        s.pending.extend_from_slice(points);
        while s.pending.len() >= block_size {
            let chunk: Vec<Point> = s.pending.drain(..block_size).collect();
            s.blocks.push(Block::build(&chunk));
        }
        Ok(())
    }

    pub fn flush(&mut self, series: &str) -> Result<()> {
        let s = self
            .series
            .get_mut(series)
            .ok_or_else(|| Error::UnknownSeries(series.to_string()))?;
        s.flush();
        Ok(())
    }

    pub fn flush_all(&mut self) {
        for s in self.series.values_mut() {
            s.flush();
        }
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

    /// Flush pending points and write a complete v2 snapshot atomically
    /// (temp file, fsync, rename, directory fsync). Legacy v1 files are
    /// upgraded in memory and become v2 after the first save.
    pub fn save(&mut self, path: &Path) -> Result<()> {
        self.flush_all();
        let bytes = encode_db(self);
        let file_name = path
            .file_name()
            .ok_or_else(|| Error::Format(format!("invalid path: {}", path.display())))?;
        let tmp = path.with_file_name(format!("{}.tmp", file_name.to_string_lossy()));

        {
            let mut f = File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        {
            if let Some(parent) = path.parent() {
                if let Ok(dir) = File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
        }
        self.format_version = FORMAT_VERSION;
        Ok(())
    }

    pub fn open(path: &Path) -> Result<Db> {
        let meta = fs::metadata(path)?;
        if meta.len() > MAX_DB_BYTES {
            return Err(Error::Format(format!(
                "database is {} bytes, limit is {MAX_DB_BYTES}",
                meta.len()
            )));
        }
        let mut data = Vec::with_capacity(meta.len() as usize);
        File::open(path)?.read_to_end(&mut data)?;
        decode_db(&data)
    }

    pub fn load_or_new(path: &Path) -> Result<Db> {
        if path.exists() {
            Db::open(path)
        } else {
            Ok(Db::new())
        }
    }
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

fn encode_db(db: &Db) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC_V2);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved flags
    out.extend_from_slice(&(db.block_size as u32).to_le_bytes());
    out.extend_from_slice(&(db.series.len() as u32).to_le_bytes());

    for s in db.series.values() {
        let name = s.name.as_bytes();
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&(s.blocks.len() as u32).to_le_bytes());

        for b in &s.blocks {
            let start = out.len();
            out.push(b.codec as u8);
            let mut flags = 0u8;
            if b.sum.is_some() {
                flags |= 1;
            }
            if b.has_nan {
                flags |= 2;
            }
            out.push(flags);
            out.extend_from_slice(&b.count.to_le_bytes());
            out.extend_from_slice(&b.t_min.to_le_bytes());
            out.extend_from_slice(&b.t_max.to_le_bytes());
            out.extend_from_slice(&b.v_min.to_le_bytes());
            out.extend_from_slice(&b.v_max.to_le_bytes());
            out.extend_from_slice(&b.sum.unwrap_or(f64::NAN).to_le_bytes());
            out.extend_from_slice(&(b.payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&b.payload);
            // v2 CRC covers the block header and the payload, so corrupt
            // metadata cannot silently produce wrong fast aggregates.
            out.extend_from_slice(&crc32(&out[start..]).to_le_bytes());
        }
    }
    out
}

fn decode_db(data: &[u8]) -> Result<Db> {
    if data.len() < 8 {
        return Err(Error::Format("truncated file header".into()));
    }
    match &data[..8] {
        m if m == MAGIC_V1 => decode_v1(data),
        m if m == MAGIC_V2 => decode_v2(data),
        _ => Err(Error::Format(
            "bad magic; not a shrunk database (expected SHRUNK01 or SHRUNK02)".into(),
        )),
    }
}

fn decode_v1(data: &[u8]) -> Result<Db> {
    let mut c = Cursor::new(data);
    c.take(8)?;
    let block_size = c.u32()? as usize;
    let series_count = c.u32()? as usize;
    let mut db = decode_body(&mut c, data, block_size, series_count, false)?;
    db.format_version = 1;
    Ok(db)
}

fn decode_v2(data: &[u8]) -> Result<Db> {
    let mut c = Cursor::new(data);
    c.take(8)?;
    let version = u16::from_le_bytes(c.take(2)?.try_into().unwrap());
    let flags = u16::from_le_bytes(c.take(2)?.try_into().unwrap());
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
    let mut db = decode_body(&mut c, data, block_size, series_count, true)?;
    db.format_version = FORMAT_VERSION;
    Ok(db)
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
        if block_count > (data.len().saturating_sub(c.pos)) / BLOCK_HEADER_BYTES {
            return Err(Error::Format(format!(
                "series {name:?}: block count {block_count} exceeds remaining file size"
            )));
        }
        let mut blocks = Vec::with_capacity(block_count);
        let mut prev_t_max: Option<i64> = None;

        for _ in 0..block_count {
            let block = read_block(c, data, &name, v2_crc)?;
            if let Some(prev) = prev_t_max {
                if block.t_min < prev {
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
        payload,
    };
    block
        .validate_meta()
        .map_err(|e| Error::Format(format!("series {name:?}: {e}")))?;
    Ok(block)
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

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

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
}
