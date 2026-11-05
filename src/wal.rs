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
    pub fn open(path: &Path, block_size: usize) -> Result<Wal> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let len = file.metadata()?.len();
        if len < WAL_HEADER_BYTES {
            // A crash while creating the header leaves a short file. Treat it
            // as empty and rewrite the header.
            file.set_len(0)?;
            write_header(&mut file, block_size)?;
            file.sync_data()?;
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
        let block_size = self.block_size;
        self.ensure_header(block_size)?;
        let mut payload = Vec::with_capacity(12 + series.len() + points.len() * 16);
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
        self.file.write_all(&record)?;
        self.file.sync_data()?;
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
    file.seek(SeekFrom::Start(WAL_HEADER_BYTES))?;
    let mut pos = WAL_HEADER_BYTES;
    while pos < len {
        if len - pos < 4 {
            break;
        }
        let mut len_buf = [0u8; 4];
        if file.read_exact(&mut len_buf).is_err() {
            break;
        }
        let record_len = u32::from_le_bytes(len_buf) as u64;
        if record_len > MAX_WAL_RECORD_BYTES || record_len + 8 > len - pos {
            break;
        }
        let mut payload = vec![0u8; record_len as usize];
        if file.read_exact(&mut payload).is_err() {
            break;
        }
        let mut crc_buf = [0u8; 4];
        if file.read_exact(&mut crc_buf).is_err() {
            break;
        }
        let stored = u32::from_le_bytes(crc_buf);
        if crc32(&payload) != stored {
            break;
        }
        let (series, points) = decode_payload(&payload)?;
        apply(&series, &points)?;
        pos += 4 + record_len + 4;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
