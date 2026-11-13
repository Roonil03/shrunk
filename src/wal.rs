//! Append-only write-ahead log for durable incremental appends.
//!
//! A WAL is a sidecar file next to the database snapshot. It contains a small
//! header and a sequence of point-batch records. Each record carries its own
//! length and CRC. An invalid *final* record (a short length prefix, a length
//! that runs past EOF, a bad CRC with no valid record after it, or a
//! zero-filled tail) is treated as a torn crash tail: readers ignore it and
//! writers truncate it. Damage followed by a later valid record is still a
//! hard error and is preserved.
//!
//! Writers take the exclusive WAL lock before the snapshot is opened, so a
//! stale writer cannot replay onto an old snapshot and overwrite a newer
//! checkpoint. Checkpoints write a normal snapshot atomically and truncate
//! the WAL. If a crash happens after the snapshot rename but before the
//! truncate, replay is idempotent: points at or before the snapshot's last
//! timestamp are skipped.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::crc::crc32;
use crate::db::{Error, Result};
use crate::Point;

const WAL_MAGIC: &[u8; 8] = b"SHRUNKWL";
const WAL_VERSION: u16 = 1;
pub(crate) const WAL_HEADER_BYTES: u64 = 16;
const RECORD_KIND_POINTS: u8 = 1;
const MAX_WAL_POINTS: usize = 16_000_000;
const MAX_WAL_RECORD_BYTES: u64 = 128 * 1024 * 1024;
// Keep recovery work bounded even when a WAL contains many individually valid
// records. Databases larger than this must be checkpointed before reopening.
const MAX_WAL_BYTES: u64 = 1024 * 1024 * 1024;
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

/// A WAL that is exclusively locked but has not yet been replayed.
///
/// Holding this keeps another writer out while the caller opens the snapshot.
#[derive(Debug)]
pub(crate) struct LockedWal {
    file: File,
    created: bool,
    path: PathBuf,
    len: u64,
}

impl LockedWal {
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Validate or recreate the header, replay complete records, and truncate
    /// a torn terminal record. Returns a still-locked writer ready to append.
    pub(crate) fn recover<F>(
        self,
        expected_block_size: Option<usize>,
        apply: &mut F,
    ) -> Result<(Wal, usize)>
    where
        F: FnMut(usize, &str, &[Point]) -> Result<()>,
    {
        let LockedWal {
            mut file,
            created,
            path,
            len,
        } = self;
        if len < WAL_HEADER_BYTES {
            // A partial header cannot contain a committed record. Recreate it
            // instead of blocking recovery on a torn header write.
            let block_size = expected_block_size.ok_or_else(|| {
                Error::InvalidInput("cannot initialize WAL without a block size".into())
            })?;
            file.set_len(0)?;
            write_header(&mut file, block_size)?;
            file.sync_data()?;
            if created {
                sync_parent_dir(&path)?;
            }
            return Ok((
                Wal {
                    file,
                    header_written: true,
                    block_size,
                },
                block_size,
            ));
        }
        let block_size = read_header(&mut file)?;
        if block_size == 0 {
            return Err(Error::Format("WAL has zero block size".into()));
        }
        if let Some(expected) = expected_block_size {
            if block_size != expected {
                return Err(Error::Format(format!(
                    "WAL block size {block_size} does not match database block size {expected}"
                )));
            }
        }
        let valid_end = scan_records(&mut file, len, true, &mut |series, points| {
            apply(block_size, series, points)
        })?;
        if valid_end != len {
            file.set_len(valid_end)?;
            file.sync_data()?;
        }
        if created {
            sync_parent_dir(&path)?;
        }
        Ok((
            Wal {
                file,
                header_written: true,
                block_size,
            },
            block_size,
        ))
    }
}

impl Wal {
    /// Open or create the WAL and take the exclusive lock before the caller
    /// reads the snapshot. Replay and torn-tail repair happen afterwards in
    /// [`LockedWal::recover`].
    pub(crate) fn lock_writer(path: &Path) -> Result<LockedWal> {
        let (file, created) = match open_wal(path, true, true, true) {
            Ok(file) => (file, true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                (open_wal(path, true, false, true)?, false)
            }
            Err(e) => return Err(e.into()),
        };
        lock_exclusive(&file)?;
        let len = file.metadata()?.len();
        check_wal_size(len)?;
        Ok(LockedWal {
            file,
            created,
            path: path.to_path_buf(),
            len,
        })
    }

    /// Lock the WAL, repair a torn terminal record after replay succeeds, and
    /// return the still-locked writer. Prefer [`Self::lock_writer`] then
    /// [`LockedWal::recover`] when the snapshot must be opened under the lock.
    #[cfg(test)]
    pub(crate) fn recover_and_open<F>(
        path: &Path,
        expected_block_size: Option<usize>,
        apply: &mut F,
    ) -> Result<(Wal, usize)>
    where
        F: FnMut(usize, &str, &[Point]) -> Result<()>,
    {
        Self::lock_writer(path)?.recover(expected_block_size, apply)
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
        let new_len = old_len
            .checked_add(record.len() as u64)
            .ok_or_else(|| Error::Limit("WAL size overflow".into()))?;
        if new_len > MAX_WAL_BYTES {
            return Err(Error::Limit(format!(
                "appending would exceed WAL recovery limit {MAX_WAL_BYTES}"
            )));
        }
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
        // Reflect the truncated file even if the durability sync fails, so a
        // later append does not write a record without a header.
        self.header_written = false;
        self.file.sync_all()?;
        Ok(())
    }

    pub fn ensure_header(&mut self, block_size: usize) -> Result<()> {
        if !self.header_written {
            write_header(&mut self.file, block_size)?;
            if let Err(error) = self.file.sync_data() {
                // The header write may have reached the file without being
                // durable. Drop it so a retry rewrites instead of appending a
                // second header.
                let _ = self.file.set_len(0);
                return Err(error.into());
            }
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

/// Open a WAL without following a final-component symlink, and reject special
/// files. New WALs are private to their owner on Unix (subject to umask).
/// Linux/Android `O_NOFOLLOW`. The value is architecture-specific: 0x8000 on
/// arm, aarch64, powerpc, and sparc; 0x20000 (asm-generic) elsewhere.
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "sparc",
        target_arch = "sparc64"
    )
))]
const LINUX_O_NOFOLLOW: i32 = 0o100000;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    not(any(
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "sparc",
        target_arch = "sparc64"
    ))
))]
const LINUX_O_NOFOLLOW: i32 = 0o400000;

fn open_wal(path: &Path, writable: bool, create_new: bool, append: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(writable).append(append);
    if create_new {
        options.create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    options.custom_flags(LINUX_O_NOFOLLOW);
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    options.custom_flags(0x100); // O_NOFOLLOW (Darwin)
    let file = options.open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "WAL path is not a regular file",
        ));
    }
    reject_wal_alias(path, &file)?;
    Ok(file)
}

/// Reject a final-component symlink even when `O_NOFOLLOW` is unavailable or
/// has the wrong value for this architecture. On Unix, also require the
/// opened descriptor to be the same inode as the path.
fn reject_wal_alias(path: &Path, file: &File) -> std::io::Result<()> {
    let path_meta = std::fs::symlink_metadata(path)?;
    if path_meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "WAL path is a symlink",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let file_meta = file.metadata()?;
        if path_meta.dev() != file_meta.dev() || path_meta.ino() != file_meta.ino() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "WAL path changed during open",
            ));
        }
    }
    let _ = file;
    Ok(())
}

fn check_wal_size(len: u64) -> Result<()> {
    if len > MAX_WAL_BYTES {
        return Err(Error::Limit(format!(
            "WAL size {len} exceeds recovery limit {MAX_WAL_BYTES}"
        )));
    }
    Ok(())
}

fn lock_exclusive(file: &File) -> Result<()> {
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => {
            Error::Locked("database is locked by another writer".into())
        }
        std::fs::TryLockError::Error(e) => e.into(),
    })
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
    let block_size = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    let max_block_size = crate::block::MAX_BLOCK_POINTS as usize;
    if !(1..=max_block_size).contains(&block_size) {
        return Err(Error::Format(format!(
            "WAL block size {block_size} outside 1..={max_block_size}"
        )));
    }
    Ok(block_size)
}

/// Block size recorded in a WAL header, used when recovering without a
/// snapshot.
pub(crate) fn read_block_size(path: &Path) -> Result<usize> {
    let mut file = open_wal(path, false, false, false)?;
    let len = file.metadata()?.len();
    check_wal_size(len)?;
    if file.metadata()?.len() < WAL_HEADER_BYTES {
        return Err(Error::Format("truncated WAL header".into()));
    }
    read_header(&mut file)
}

/// Replay complete records. A torn terminal record is ignored; damage
/// followed by a later valid record is reported and preserved.
pub(crate) fn replay<F>(path: &Path, apply: &mut F) -> Result<()>
where
    F: FnMut(&str, &[Point]) -> Result<()>,
{
    let mut file = open_wal(path, false, false, false)?;
    let len = file.metadata()?.len();
    check_wal_size(len)?;
    if len < WAL_HEADER_BYTES {
        // A partial header cannot hold a record; treat it as an empty WAL.
        return Ok(());
    }
    let block_size = read_header(&mut file)?;
    if block_size == 0 {
        return Err(Error::Format("WAL has zero block size".into()));
    }
    scan_records(&mut file, len, true, apply)?;
    Ok(())
}

/// Walk complete records. An invalid record at EOF (short length prefix,
/// length past EOF, bad CRC, or an undecodable tail such as a zero-filled
/// empty record) is treated as a torn crash tail and is not applied.
/// Damage followed by a later valid record is a hard error.
/// Returns the byte offset after the last complete record.
fn scan_records<F>(file: &mut File, len: u64, decode: bool, apply: &mut F) -> Result<u64>
where
    F: FnMut(&str, &[Point]) -> Result<()>,
{
    check_wal_size(len)?;
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
        let total = record_len + 8;
        if record_len > MAX_WAL_RECORD_BYTES {
            if total > remaining {
                // A torn or garbage length prefix at EOF cannot be a
                // committed record.
                break;
            }
            return Err(Error::Format(format!(
                "WAL record size {record_len} exceeds limit {MAX_WAL_RECORD_BYTES}"
            )));
        }
        if total > remaining {
            break;
        }
        let mut payload = vec![0u8; record_len as usize];
        file.read_exact(&mut payload)?;
        let mut crc_buf = [0u8; 4];
        file.read_exact(&mut crc_buf)?;
        let stored = u32::from_le_bytes(crc_buf);
        let crc_ok = crc32(&payload) == stored;
        if !crc_ok {
            if has_valid_record_after(file, pos + total, len)? {
                return Err(Error::Format(format!(
                    "WAL checksum mismatch at byte {pos}; damaged data preserved"
                )));
            }
            break;
        }
        if decode {
            match decode_payload(&payload) {
                Ok((series, points)) => apply(&series, &points)?,
                Err(error) => {
                    if has_valid_record_after(file, pos + total, len)? {
                        return Err(error);
                    }
                    break;
                }
            }
        }
        pos += total;
    }
    Ok(pos)
}

/// True if a complete, checksum-valid, decodable record exists at or after
/// `start`. Used to tell a torn tail from mid-file corruption.
fn has_valid_record_after(file: &mut File, mut pos: u64, len: u64) -> Result<bool> {
    while pos < len {
        let remaining = len - pos;
        if remaining < 8 {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(pos))?;
        let mut len_buf = [0u8; 4];
        file.read_exact(&mut len_buf)?;
        let record_len = u32::from_le_bytes(len_buf) as u64;
        if record_len > MAX_WAL_RECORD_BYTES {
            return Ok(false);
        }
        let total = record_len + 8;
        if total > remaining {
            return Ok(false);
        }
        let mut payload = vec![0u8; record_len as usize];
        file.read_exact(&mut payload)?;
        let mut crc_buf = [0u8; 4];
        file.read_exact(&mut crc_buf)?;
        if crc32(&payload) == u32::from_le_bytes(crc_buf) && decode_payload(&payload).is_ok() {
            return Ok(true);
        }
        pos += total;
    }
    Ok(false)
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

    fn open_writer(path: &Path, block_size: usize) -> Wal {
        Wal::recover_and_open(path, Some(block_size), &mut |_, _, _| Ok(()))
            .expect("open writer")
            .0
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

        let mut wal = open_writer(&path, 64);
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
    fn recover_and_open_truncates_a_bad_crc_at_eof() {
        let path = test_path("bad-tail-crc");
        remove(&path);
        let first = Point { ts: 1, value: 2.0 };
        let second = Point { ts: 2, value: 3.0 };
        let mut bytes = Vec::new();
        write_header_to_vec(&mut bytes, 64);
        bytes.extend_from_slice(&encoded_record("cpu", &[first]));
        bytes.extend_from_slice(&encoded_record("cpu", &[second]));
        *bytes.last_mut().unwrap() ^= 0x80;
        std::fs::write(&path, &bytes).unwrap();

        let mut replayed = Vec::new();
        let (wal, _) = Wal::recover_and_open(&path, Some(64), &mut |_, series, points| {
            replayed.extend(points.iter().map(|p| (series.to_owned(), *p)));
            Ok(())
        })
        .unwrap();
        drop(wal);
        assert_eq!(replayed, vec![("cpu".into(), first)]);
        assert!(std::fs::read(&path).unwrap().len() < bytes.len());

        let mut points_only = Vec::new();
        replay(&path, &mut |_, points| {
            points_only.extend(points.iter().copied());
            Ok(())
        })
        .unwrap();
        assert_eq!(points_only, vec![first]);
        remove(&path);
    }

    #[test]
    fn replay_ignores_a_zero_filled_tail() {
        let path = test_path("zero-tail");
        remove(&path);
        let first = Point { ts: 1, value: 2.0 };
        let mut bytes = Vec::new();
        write_header_to_vec(&mut bytes, 64);
        bytes.extend_from_slice(&encoded_record("cpu", &[first]));
        bytes.extend_from_slice(&[0u8; 16]);
        std::fs::write(&path, &bytes).unwrap();

        let mut replayed = Vec::new();
        replay(&path, &mut |_, points| {
            replayed.extend(points.iter().copied());
            Ok(())
        })
        .unwrap();
        assert_eq!(replayed, vec![first]);
        // Readers leave the torn tail in place.
        assert_eq!(std::fs::read(&path).unwrap().len(), bytes.len());
        remove(&path);
    }

    #[test]
    fn wal_lock_rejects_a_second_writer() {
        let path = test_path("single-writer");
        remove(&path);
        let wal = open_writer(&path, 64);
        assert!(Wal::recover_and_open(&path, Some(64), &mut |_, _, _| Ok(())).is_err());
        drop(wal);
        assert!(Wal::recover_and_open(&path, Some(64), &mut |_, _, _| Ok(())).is_ok());
        remove(&path);
    }

    #[test]
    fn append_cannot_create_a_wal_that_recovery_will_reject() {
        let path = test_path("append-recovery-bound");
        remove(&path);
        let mut wal = open_writer(&path, 64);
        wal.file.set_len(MAX_WAL_BYTES).unwrap();
        assert!(wal
            .append("cpu", &[Point { ts: 1, value: 2.0 }])
            .unwrap_err()
            .to_string()
            .contains("recovery limit"));
        drop(wal);
        remove(&path);
    }

    #[test]
    fn wal_header_rejects_out_of_range_block_size() {
        let path = test_path("bad-block-size");
        remove(&path);
        let mut bytes = Vec::new();
        write_header_to_vec(&mut bytes, 64);
        bytes[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let error = Wal::recover_and_open(&path, None, &mut |_, _, _| Ok(())).unwrap_err();
        assert!(error.to_string().contains("block size"), "{error}");
        remove(&path);
    }

    #[test]
    fn partial_wal_header_is_recreated() {
        let path = test_path("partial-header");
        remove(&path);
        std::fs::write(&path, &WAL_MAGIC[..8]).unwrap();

        let (mut wal, block_size) =
            Wal::recover_and_open(&path, Some(64), &mut |_, _, _| Ok(())).unwrap();
        assert_eq!(block_size, 64);
        wal.append("cpu", &[Point { ts: 1, value: 2.0 }]).unwrap();
        drop(wal);

        let mut replayed = Vec::new();
        replay(&path, &mut |series, points| {
            replayed.push((series.to_string(), points.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            replayed,
            vec![("cpu".into(), vec![Point { ts: 1, value: 2.0 }])]
        );
        remove(&path);
    }

    #[cfg(unix)]
    #[test]
    fn wal_creation_is_private_and_symlinks_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let path = test_path("permissions");
        remove(&path);
        drop(open_writer(&path, 64));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // O_NOFOLLOW is set on Linux, Android, and Darwin targets.
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))]
        {
            use std::os::unix::fs::symlink;
            let link = test_path("symlink");
            remove(&link);
            symlink(&path, &link).unwrap();
            assert!(Wal::recover_and_open(&link, Some(64), &mut |_, _, _| Ok(())).is_err());
            remove(&link);
        }
        remove(&path);
    }

    #[test]
    fn append_and_decode_enforce_name_and_point_limits() {
        let path = test_path("limits");
        remove(&path);
        let mut wal = open_writer(&path, 64);
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
