//! Append-only write-ahead log for durable incremental appends.
//!
//! A WAL is a sidecar file next to the database snapshot. It contains a small
//! header and a sequence of point-batch records. Each record carries its own
//! length and CRC. Replay stops at the first short or corrupt record, which is
//! how a torn tail from a crash is recovered.
//!
//! Checkpoints write a normal snapshot atomically and truncate the WAL. If a
//! crash happens after the snapshot rename but before the truncate, replay is
//! idempotent: points at or before the snapshot's last timestamp are skipped.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::crc::crc32;
use crate::db::{Error, Result};
use crate::Point;

const WAL_MAGIC: &[u8; 8] = b"SHRUNKWL";
const WAL_VERSION: u16 = 1;
pub(crate) const WAL_HEADER_BYTES: u64 = 16;
const RECORD_KIND_POINTS: u8 = 1;
const MAX_WAL_POINTS: usize = 16_000_000;
const MAX_WAL_RECORD_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SERIES_NAME: usize = 1024;

pub(crate) fn wal_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(".wal");
    PathBuf::from(name)
}

#[derive(Debug)]
pub(crate) struct Wal {
    file: File,
    header_written: bool,
    block_size: usize,
}

impl Wal {
    /// Open or create a WAL and validate its header. The block size is stored
    /// in the header so a database can be recovered even before its first
    /// snapshot exists.
    #[cfg(test)]
    pub fn open(path: &Path, block_size: usize) -> Result<Wal> {
        Self::open_inner(path, block_size, false)
    }

    /// Open a WAL whose records were already replayed and its torn suffix
    /// repaired by `replay_repair`.
    pub fn open_repaired(path: &Path, block_size: usize) -> Result<Wal> {
        Self::open_inner(path, block_size, true)
    }

    fn open_inner(path: &Path, block_size: usize, already_repaired: bool) -> Result<Wal> {
        let (mut file, created) = match OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(path)
        {
            Ok(file) => (file, true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (
                OpenOptions::new().read(true).append(true).open(path)?,
                false,
            ),
            Err(e) => return Err(e.into()),
        };
        let len = file.metadata()?.len();
        if len < WAL_HEADER_BYTES {
            // A crash while creating the header leaves a short file. Treat it
            // as empty and rewrite the header.
            file.set_len(0)?;
            write_header(&mut file, block_size)?;
            file.sync_data()?;
            if created {
                sync_parent_dir(path)?;
            }
            return Ok(Wal {
                file,
                header_written: true,
                block_size,
            });
        }
        let found = read_header(&mut file)?;
        if found != block_size {
            return Err(Error::Format(format!(
                "WAL block size {found} does not match database block size {block_size}"
            )));
        }
        if !already_repaired {
            let valid_end = scan_records(&mut file, len, false, &mut |_, _| Ok(()))?;
            if valid_end != len {
                file.set_len(valid_end)?;
                file.sync_data()?;
            }
        }
        if created {
            sync_parent_dir(path)?;
        }
        Ok(Wal {
            file,
            header_written: true,
            block_size,
        })
    }

    /// Append one durable batch. The record is fsynced before the caller
    /// applies it to memory, so a crash cannot lose an acknowledged batch.
    pub fn append(&mut self, series: &str, points: &[Point]) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let payload_len = checked_payload_len(series.len(), points.len())?;
        let block_size = self.block_size;
        self.ensure_header(block_size)?;
        let mut payload = Vec::with_capacity(payload_len);
        payload.push(RECORD_KIND_POINTS);
        put_u32(&mut payload, series.len() as u32);
        put_u32(&mut payload, points.len() as u32);
        payload.extend_from_slice(series.as_bytes());
        for p in points {
            payload.extend_from_slice(&p.ts.to_le_bytes());
            payload.extend_from_slice(&p.value.to_bits().to_le_bytes());
        }
        let mut record = Vec::with_capacity(payload.len() + 8);
        put_u32(&mut record, payload.len() as u32);
        record.extend_from_slice(&payload);
        put_u32(&mut record, crc32(&payload));
        let old_len = self.file.metadata()?.len();
        if let Err(error) = self.file.write_all(&record) {
            self.file.set_len(old_len)?;
            self.file.sync_data()?;
            return Err(error.into());
        }
        if let Err(error) = self.file.sync_data() {
            self.file.set_len(old_len)?;
            self.file.sync_data()?;
            return Err(error.into());
        }
        Ok(())
    }

    /// Empty the WAL after a successful snapshot checkpoint. The next append
    /// recreates the header.
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.sync_all()?;
        self.header_written = false;
        Ok(())
    }

    pub fn ensure_header(&mut self, block_size: usize) -> Result<()> {
        if !self.header_written {
            write_header(&mut self.file, block_size)?;
            self.file.sync_data()?;
            self.header_written = true;
        }
        Ok(())
    }
}

fn write_header(file: &mut File, block_size: usize) -> Result<()> {
    let mut header = Vec::with_capacity(WAL_HEADER_BYTES as usize);
    header.extend_from_slice(WAL_MAGIC);
    header.extend_from_slice(&WAL_VERSION.to_le_bytes());
    header.extend_from_slice(&0u16.to_le_bytes());
    put_u32(&mut header, block_size as u32);
    file.write_all(&header)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<()> {
    Ok(())
}

fn read_header(file: &mut File) -> Result<usize> {
    file.seek(SeekFrom::Start(0))?;
    let mut header = [0u8; WAL_HEADER_BYTES as usize];
    file.read_exact(&mut header)?;
    if &header[..8] != WAL_MAGIC {
        return Err(Error::Format("bad WAL magic".into()));
    }
    let version = u16::from_le_bytes(header[8..10].try_into().unwrap());
    let flags = u16::from_le_bytes(header[10..12].try_into().unwrap());
    if version != WAL_VERSION {
        return Err(Error::Format(format!(
            "unsupported WAL version {version}; expected {WAL_VERSION}"
        )));
    }
    if flags != 0 {
        return Err(Error::Format(format!("unsupported WAL flags {flags:#x}")));
    }
    Ok(u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize)
}

/// Block size recorded in a WAL header, used when recovering without a
/// snapshot.
pub(crate) fn read_block_size(path: &Path) -> Result<usize> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() < WAL_HEADER_BYTES {
        return Err(Error::Format("truncated WAL header".into()));
    }
    read_header(&mut file)
}

/// Replay complete records. A short or CRC-failing record at the tail is
/// treated as a torn write and ignored; earlier records are still applied.
pub(crate) fn replay<F>(path: &Path, apply: &mut F) -> Result<()>
where
    F: FnMut(&str, &[Point]) -> Result<()>,
{
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < WAL_HEADER_BYTES {
        return Ok(());
    }
    let block_size = read_header(&mut file)?;
    if block_size == 0 {
        return Err(Error::Format("WAL has zero block size".into()));
    }
    scan_records(&mut file, len, true, apply)?;
    Ok(())
}

/// Replay complete records and truncate any torn terminal suffix before the
/// file is reopened for append.
pub(crate) fn replay_repair<F>(path: &Path, apply: &mut F) -> Result<()>
where
    F: FnMut(&str, &[Point]) -> Result<()>,
{
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    if len < WAL_HEADER_BYTES {
        file.set_len(0)?;
        file.sync_data()?;
        return Ok(());
    }
    let block_size = read_header(&mut file)?;
    if block_size == 0 {
        return Err(Error::Format("WAL has zero block size".into()));
    }
    let valid_end = scan_records(&mut file, len, true, apply)?;
    if valid_end != len {
        file.set_len(valid_end)?;
        file.sync_data()?;
    }
    Ok(())
}

/// Walk complete records. An incomplete or checksum-invalid terminal record
/// is a torn suffix; equivalent damage before later bytes is corruption.
/// Returns the byte offset after the last complete record.
fn scan_records<F>(file: &mut File, len: u64, decode: bool, apply: &mut F) -> Result<u64>
where
    F: FnMut(&str, &[Point]) -> Result<()>,
{
    file.seek(SeekFrom::Start(WAL_HEADER_BYTES))?;
    let mut pos = WAL_HEADER_BYTES;
    while pos < len {
        let remaining = len - pos;
        if remaining < 4 {
            break;
        }
        let mut len_buf = [0u8; 4];
        file.read_exact(&mut len_buf)?;
        let record_len = u32::from_le_bytes(len_buf) as u64;
        if record_len > MAX_WAL_RECORD_BYTES {
            return Err(Error::Format(format!(
                "WAL record size {record_len} exceeds limit {MAX_WAL_RECORD_BYTES}"
            )));
        }
        let total = record_len + 8;
        if total > remaining {
            break;
        }
        let mut payload = vec![0u8; record_len as usize];
        file.read_exact(&mut payload)?;
        let mut crc_buf = [0u8; 4];
        file.read_exact(&mut crc_buf)?;
        let stored = u32::from_le_bytes(crc_buf);
        if crc32(&payload) != stored {
            if total < remaining {
                return Err(Error::Format(format!(
                    "WAL checksum mismatch at byte {pos} before trailing records"
                )));
            }
            break;
        }
        if decode {
            let (series, points) = decode_payload(&payload)?;
            apply(&series, &points)?;
        }
        pos += total;
    }
    Ok(pos)
}

fn decode_payload(payload: &[u8]) -> Result<(String, Vec<Point>)> {
    if payload.len() < 9 {
        return Err(Error::Format("WAL record is too short".into()));
    }
    if payload[0] != RECORD_KIND_POINTS {
        return Err(Error::Format(format!(
            "unknown WAL record kind {}",
            payload[0]
        )));
    }
    let name_len = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
    let count = u32::from_le_bytes(payload[5..9].try_into().unwrap()) as usize;
    if name_len > MAX_SERIES_NAME {
        return Err(Error::Format(format!(
            "series name length {name_len} exceeds limit {MAX_SERIES_NAME}"
        )));
    }
    if count > MAX_WAL_POINTS {
        return Err(Error::Format(format!(
            "WAL record has {count} points, limit is {MAX_WAL_POINTS}"
        )));
    }
    let expected = 9usize
        .checked_add(name_len)
        .and_then(|n| n.checked_add(count.checked_mul(16)?))
        .ok_or_else(|| Error::Format("WAL record size overflow".into()))?;
    if expected != payload.len() {
        return Err(Error::Format(format!(
            "WAL record length mismatch: expected {expected}, got {}",
            payload.len()
        )));
    }
    let name_end = 9 + name_len;
    let series = String::from_utf8(payload[9..name_end].to_vec())
        .map_err(|_| Error::Format("WAL series name is not utf-8".into()))?;
    let mut points = Vec::with_capacity(count);
    let mut pos = name_end;
    for _ in 0..count {
        let ts = i64::from_le_bytes(payload[pos..pos + 8].try_into().unwrap());
        let bits = u64::from_le_bytes(payload[pos + 8..pos + 16].try_into().unwrap());
        points.push(Point {
            ts,
            value: f64::from_bits(bits),
        });
        pos += 16;
    }
    Ok((series, points))
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn checked_payload_len(name_len: usize, count: usize) -> Result<usize> {
    if name_len > MAX_SERIES_NAME {
        return Err(Error::Format(format!(
            "series name length {name_len} exceeds limit {MAX_SERIES_NAME}"
        )));
    }
    if count > MAX_WAL_POINTS {
        return Err(Error::Format(format!(
            "WAL record has {count} points, limit is {MAX_WAL_POINTS}"
        )));
    }
    let payload_len = 9usize
        .checked_add(name_len)
        .and_then(|n| n.checked_add(count.checked_mul(16)?))
        .ok_or_else(|| Error::Format("WAL record size overflow".into()))?;
    if payload_len as u64 > MAX_WAL_RECORD_BYTES || payload_len > u32::MAX as usize {
        return Err(Error::Format(format!(
            "WAL record size {payload_len} exceeds limit {MAX_WAL_RECORD_BYTES}"
        )));
    }
    Ok(payload_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shrunk-wal-unit-{}-{label}.wal",
            std::process::id()
        ))
    }

    fn encoded_record(series: &str, points: &[Point]) -> Vec<u8> {
        let mut payload = vec![RECORD_KIND_POINTS];
        put_u32(&mut payload, series.len() as u32);
        put_u32(&mut payload, points.len() as u32);
        payload.extend_from_slice(series.as_bytes());
        for point in points {
            payload.extend_from_slice(&point.ts.to_le_bytes());
            payload.extend_from_slice(&point.value.to_bits().to_le_bytes());
        }
        let mut record = Vec::new();
        put_u32(&mut record, payload.len() as u32);
        record.extend_from_slice(&payload);
        put_u32(&mut record, crc32(&payload));
        record
    }

    fn remove(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn payload_roundtrip() {
        let points = vec![
            Point { ts: 1, value: 2.5 },
            Point {
                ts: 2,
                value: f64::NAN,
            },
        ];
        let mut payload = vec![RECORD_KIND_POINTS];
        put_u32(&mut payload, 3);
        put_u32(&mut payload, 2);
        payload.extend_from_slice(b"cpu");
        for p in &points {
            payload.extend_from_slice(&p.ts.to_le_bytes());
            payload.extend_from_slice(&p.value.to_bits().to_le_bytes());
        }
        let (series, back) = decode_payload(&payload).unwrap();
        assert_eq!(series, "cpu");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], points[0]);
        assert_eq!(back[1].value.to_bits(), points[1].value.to_bits());
    }

    #[test]
    fn open_repairs_torn_suffix_before_future_append() {
        let path = test_path("repair");
        remove(&path);
        let first = Point { ts: 1, value: 2.0 };
        let second = Point { ts: 2, value: 3.0 };
        let mut bytes = Vec::new();
        write_header_to_vec(&mut bytes, 64);
        bytes.extend_from_slice(&encoded_record("cpu", &[first]));
        bytes.extend_from_slice(&[9, 0, 0]); // torn length prefix
        std::fs::write(&path, &bytes).unwrap();

        let mut wal = Wal::open(&path, 64).unwrap();
        wal.append("cpu", &[second]).unwrap();
        drop(wal);

        let mut replayed = Vec::new();
        replay(&path, &mut |name, points| {
            replayed.extend(points.iter().map(|p| (name.to_owned(), p.ts)));
            Ok(())
        })
        .unwrap();
        assert_eq!(replayed, vec![("cpu".into(), 1), ("cpu".into(), 2)]);
        remove(&path);
    }

    #[test]
    fn replay_rejects_corruption_before_later_records() {
        let path = test_path("mid-corruption");
        remove(&path);
        let record = encoded_record("cpu", &[Point { ts: 1, value: 2.0 }]);
        let mut bytes = Vec::new();
        write_header_to_vec(&mut bytes, 64);
        bytes.extend_from_slice(&record);
        bytes.extend_from_slice(&encoded_record("cpu", &[Point { ts: 2, value: 3.0 }]));
        bytes[WAL_HEADER_BYTES as usize + record.len() - 1] ^= 1;
        std::fs::write(&path, bytes).unwrap();

        let error = replay(&path, &mut |_, _| Ok(())).unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        remove(&path);
    }

    #[test]
    fn append_and_decode_enforce_name_and_point_limits() {
        let path = test_path("limits");
        remove(&path);
        let mut wal = Wal::open(&path, 64).unwrap();
        let long_name = "x".repeat(MAX_SERIES_NAME + 1);
        assert!(wal
            .append(&long_name, &[Point { ts: 1, value: 1.0 }])
            .is_err());
        assert!(checked_payload_len(3, MAX_WAL_POINTS + 1).is_err());
        assert!(checked_payload_len(3, usize::MAX).is_err());

        let mut payload = vec![RECORD_KIND_POINTS];
        put_u32(&mut payload, (MAX_SERIES_NAME + 1) as u32);
        put_u32(&mut payload, 0);
        payload.extend(std::iter::repeat_n(b'x', MAX_SERIES_NAME + 1));
        assert!(decode_payload(&payload).is_err());
        drop(wal);
        remove(&path);
    }

    fn write_header_to_vec(out: &mut Vec<u8>, block_size: usize) {
        out.extend_from_slice(WAL_MAGIC);
        out.extend_from_slice(&WAL_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        put_u32(out, block_size as u32);
    }
}
