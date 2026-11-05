use std::fs::File;
#[cfg(not(unix))]
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use crate::codec::{dolphin_skip_rate, predictor_is_promising, Codec};
use crate::crc::crc32;
use crate::Point;

/// Bytes of block header written to disk, excluding the payload and trailing
/// payload checksum:
/// codec(1) + flags(1) + count(4) + t_min(8) + t_max(8) + v_min(8) +
/// v_max(8) + sum(8) + payload_len(4) + header_crc(4).
pub const BLOCK_HEADER_BYTES: usize = 54;

/// Upper bounds used when parsing untrusted files. They are deliberately
/// generous for real workloads but stop a corrupt length field from causing
/// unbounded allocation.
pub const MAX_BLOCK_POINTS: u32 = 1_048_576;
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Where a block's compressed bytes live.
#[derive(Clone, Debug)]
pub enum Payload {
    /// Payload built in memory and not yet written to a file.
    Memory(Vec<u8>),
    /// Payload in an open snapshot file, read on demand.
    File {
        file: Arc<File>,
        offset: u64,
        len: u32,
    },
}

impl Payload {
    pub fn len(&self) -> usize {
        match self {
            Payload::Memory(bytes) => bytes.len(),
            Payload::File { len, .. } => *len as usize,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read the payload. File-backed blocks allocate only for the block being
    /// decoded, which keeps open and query memory bounded.
    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        match self {
            Payload::Memory(bytes) => Ok(bytes.clone()),
            Payload::File { file, offset, len } => {
                let mut buf = vec![0u8; *len as usize];
                read_at(file, *offset, &mut buf)?;
                Ok(buf)
            }
        }
    }

    fn with_bytes<T>(&self, f: impl FnOnce(&[u8]) -> T) -> Result<T, String> {
        match self {
            Payload::Memory(bytes) => Ok(f(bytes)),
            Payload::File { .. } => {
                let bytes = self.bytes()?;
                Ok(f(&bytes))
            }
        }
    }
}

pub(crate) fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
            .map_err(|e| format!("read block payload at {offset}: {e}"))
    }
    #[cfg(not(unix))]
    {
        let mut f = file
            .try_clone()
            .map_err(|e| format!("clone block file: {e}"))?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("seek block payload at {offset}: {e}"))?;
        f.read_exact(buf)
            .map_err(|e| format!("read block payload at {offset}: {e}"))
    }
}

/// A compressed, immutable run of points plus the metadata queries need to
/// decide whether decoding it is necessary.
#[derive(Clone, Debug)]
pub struct Block {
    pub codec: Codec,
    pub count: u32,
    pub t_min: i64,
    pub t_max: i64,
    pub v_min: f64,
    pub v_max: f64,
    pub has_nan: bool,
    pub sum: Option<f64>,
    pub payload: Payload,
    /// Expected payload-only CRC from a v3 file. Checked lazily on decode.
    pub payload_crc: Option<u32>,
}

impl Block {
    pub fn build(points: &[Point]) -> Block {
        assert!(!points.is_empty(), "cannot build an empty block");

        let mut t_min = points[0].ts;
        let mut t_max = points[0].ts;
        let mut v_min = f64::INFINITY;
        let mut v_max = f64::NEG_INFINITY;
        let mut has_nan = false;
        let mut all_finite = true;
        let mut sum = 0.0f64;

        for p in points {
            t_min = t_min.min(p.ts);
            t_max = t_max.max(p.ts);
            if p.value.is_nan() {
                has_nan = true;
                all_finite = false;
            } else {
                v_min = v_min.min(p.value);
                v_max = v_max.max(p.value);
                if p.value.is_finite() {
                    sum += p.value;
                } else {
                    all_finite = false;
                }
            }
        }

        let mut best_codec = Codec::Raw;
        let mut best = Codec::Raw.encode(points).expect("raw always encodes");
        // Dolphin only pays off on oscillating blocks. Skip its extra bit per
        // new window elsewhere, where it cannot beat Gorilla.
        let use_dolphin = dolphin_skip_rate(points) >= 0.08;
        // Predictor tables only pay off on predictable blocks. A short sample
        // encode decides whether trying the full block is worthwhile.
        let use_predictor = predictor_is_promising(points);
        for codec in Codec::CANDIDATES {
            if codec == Codec::Dolphin && !use_dolphin {
                continue;
            }
            if codec == Codec::Predictor && !use_predictor {
                continue;
            }
            if let Some(bytes) = codec.encode(points) {
                if bytes.len() < best.len() {
                    best_codec = codec;
                    best = bytes;
                }
            }
        }

        Block {
            codec: best_codec,
            count: points.len() as u32,
            t_min,
            t_max,
            v_min,
            v_max,
            has_nan,
            // Sum can overflow to infinity for huge finite inputs. Treat that
            // as "no trusted sum" rather than persisting an unusable value.
            sum: if all_finite && sum.is_finite() {
                Some(sum)
            } else {
                None
            },
            payload: Payload::Memory(best),
            payload_crc: None,
        }
    }

    /// Check metadata read from disk before it can influence query planning.
    /// Payload bytes are not decoded here.
    pub fn validate_meta(&self) -> Result<(), String> {
        if self.count == 0 {
            return Err("block with zero points".into());
        }
        if self.count > MAX_BLOCK_POINTS {
            return Err(format!(
                "block has {} points, limit is {MAX_BLOCK_POINTS}",
                self.count
            ));
        }
        if self.t_min > self.t_max {
            return Err(format!(
                "block time bounds inverted: t_min {} > t_max {}",
                self.t_min, self.t_max
            ));
        }
        if self.payload.is_empty() {
            return Err("block has an empty payload".into());
        }
        if self.payload.len() > MAX_PAYLOAD_BYTES {
            return Err(format!(
                "block payload is {} bytes, limit is {MAX_PAYLOAD_BYTES}",
                self.payload.len()
            ));
        }
        if !self.has_nan && self.v_min > self.v_max {
            return Err(format!(
                "block value bounds inverted: v_min {} > v_max {}",
                self.v_min, self.v_max
            ));
        }
        if let Some(sum) = self.sum {
            if !sum.is_finite() {
                return Err("block sum is not finite".into());
            }
        }
        let min_payload = match self.codec {
            Codec::Raw => (self.count as usize)
                .checked_mul(16)
                .ok_or_else(|| "raw block size overflow".to_string())?,
            _ => 1,
        };
        if self.payload.len() < min_payload {
            return Err(format!(
                "{} payload is {} bytes, need at least {} for {} points",
                self.codec.name(),
                self.payload.len(),
                min_payload,
                self.count
            ));
        }
        if self.codec == Codec::Raw && self.payload.len() != min_payload {
            return Err(format!(
                "raw payload is {} bytes, expected {}",
                self.payload.len(),
                min_payload
            ));
        }
        Ok(())
    }

    pub fn decode(&self) -> Result<Vec<Point>, String> {
        let points = self.payload.with_bytes(|bytes| {
            if let Some(expected) = self.payload_crc {
                let actual = crc32(bytes);
                if actual != expected {
                    return Err(format!(
                        "payload checksum mismatch: stored {expected:#010x}, got {actual:#010x}"
                    ));
                }
            }
            self.codec.decode(bytes, self.count as usize)
        })??;
        self.validate_decoded(&points)?;
        Ok(points)
    }

    /// Verify that a payload's decoded contents agree with the header that was
    /// used to plan the query. This catches corruption that a payload CRC
    /// cannot catch in legacy files, and metadata/codec disagreements.
    pub fn validate_decoded(&self, points: &[Point]) -> Result<(), String> {
        if points.len() != self.count as usize {
            return Err(format!(
                "decoded {} points, header says {}",
                points.len(),
                self.count
            ));
        }
        let bounded_values = !self.has_nan || self.v_min <= self.v_max;
        for (i, p) in points.iter().enumerate() {
            if p.ts < self.t_min || p.ts > self.t_max {
                return Err(format!(
                    "point {i} timestamp {} outside block bounds [{}, {}]",
                    p.ts, self.t_min, self.t_max
                ));
            }
            if p.value.is_nan() {
                if !self.has_nan {
                    return Err(format!("point {i} is NaN but block has_nan is false"));
                }
            } else if bounded_values && (p.value < self.v_min || p.value > self.v_max) {
                return Err(format!(
                    "point {i} value {} outside block bounds [{}, {}]",
                    p.value, self.v_min, self.v_max
                ));
            }
        }
        Ok(())
    }

    pub fn stored_bytes(&self) -> usize {
        BLOCK_HEADER_BYTES + self.payload.len() + 4
    }

    pub fn raw_bytes(&self) -> u64 {
        self.count as u64 * 16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::{generate, Pattern};

    #[test]
    fn dolphin_is_selected_only_for_oscillating_blocks() {
        let oscillating = generate(Pattern::Oscillating, 1024, 1_700_000_000_000, 1000);
        assert_eq!(Block::build(&oscillating).codec, Codec::Dolphin);

        let cpu = generate(Pattern::Cpu, 1024, 1_700_000_000_000, 1000);
        assert_ne!(Block::build(&cpu).codec, Codec::Dolphin);
    }
}
