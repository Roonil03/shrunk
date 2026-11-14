//! Block codecs.
//!
//! Every block holds a run of `(timestamp, value)` points encoded with one of:
//!
//! * [`Codec::Gorilla`] - delta-of-delta timestamps and XOR floats, the format
//!   from Facebook's Gorilla paper. Good default for gauges.
//! * [`Codec::Delta`] - zigzag varint deltas for integral values. Good for
//!   counters and monotonic metrics.
//! * [`Codec::Rle`] - runs of equal values at a fixed step. Good for constants
//!   and flat sensors.
//! * [`Codec::Raw`] - 16 bytes per point. Fallback when the others lose.
//!
//! A block is compressed with all applicable codecs and the smallest result
//! wins. The block header keeps `t_min/t_max`, `v_min/v_max`, a sum, and a NaN
//! flag so queries can skip blocks without decoding them.

use crate::bits::{BitReader, BitWriter};
use crate::Point;

/// Keep public codec decoding bounded even when called outside `Block`.
pub(crate) const MAX_DECODE_POINTS: usize = 1_048_576;

fn point_vec(capacity: usize, codec: &str) -> Result<Vec<Point>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(capacity)
        .map_err(|e| format!("{codec}: cannot allocate output for {capacity} points: {e}"))?;
    Ok(out)
}

fn value_vec(capacity: usize, codec: &str) -> Result<Vec<f64>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(capacity)
        .map_err(|e| format!("{codec}: cannot allocate values for {capacity} points: {e}"))?;
    Ok(out)
}

fn timestamp_vec(capacity: usize, codec: &str) -> Result<Vec<i64>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(capacity)
        .map_err(|e| format!("{codec}: cannot allocate timestamps for {capacity} points: {e}"))?;
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Codec {
    Raw = 0,
    Gorilla = 1,
    Delta = 2,
    Rle = 3,
    /// Gorilla XOR values with delta-of-delta run-length timestamps (GR).
    GorillaRle = 4,
    /// Dolphin-inspired skip encoding for oscillating XOR prefixes.
    Dolphin = 5,
    /// FCM/DFCM predictor residuals (AFC/FPC-inspired prototype).
    Predictor = 6,
}

impl Codec {
    /// Every persisted codec. Used by research benchmarks and round-trip tests.
    pub const ALL: [Codec; 7] = [
        Codec::Raw,
        Codec::Gorilla,
        Codec::Delta,
        Codec::Rle,
        Codec::GorillaRle,
        Codec::Dolphin,
        Codec::Predictor,
    ];

    /// Order matters: on a size tie the earlier codec wins.
    pub const CANDIDATES: [Codec; 6] = [
        Codec::GorillaRle,
        Codec::Dolphin,
        Codec::Predictor,
        Codec::Gorilla,
        Codec::Delta,
        Codec::Rle,
    ];

    pub fn from_u8(v: u8) -> Option<Codec> {
        match v {
            0 => Some(Codec::Raw),
            1 => Some(Codec::Gorilla),
            2 => Some(Codec::Delta),
            3 => Some(Codec::Rle),
            4 => Some(Codec::GorillaRle),
            5 => Some(Codec::Dolphin),
            6 => Some(Codec::Predictor),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::Raw => "raw",
            Codec::Gorilla => "gorilla",
            Codec::Delta => "delta-varint",
            Codec::Rle => "rle",
            Codec::GorillaRle => "gorilla-rle",
            Codec::Dolphin => "dolphin",
            Codec::Predictor => "predictor",
        }
    }

    /// Encode a non-empty slice. Returns `None` when the codec does not apply.
    pub fn encode(self, points: &[Point]) -> Option<Vec<u8>> {
        if points.is_empty() {
            return None;
        }
        match self {
            Codec::Raw => Some(raw_encode(points)),
            Codec::Gorilla => Some(gorilla_encode(points)),
            Codec::Delta => delta_encode(points),
            Codec::Rle => Some(rle_encode(points)),
            Codec::GorillaRle => Some(gorilla_rle_encode(points)),
            Codec::Dolphin => Some(dolphin_encode(points)),
            Codec::Predictor => Some(predictor_encode(points)),
        }
    }

    pub fn decode(self, bytes: &[u8], count: usize) -> Result<Vec<Point>, String> {
        if count == 0 {
            return if bytes.is_empty() {
                Ok(Vec::new())
            } else {
                Err(format!(
                    "{}: payload for zero points is not empty",
                    self.name()
                ))
            };
        }
        if count > MAX_DECODE_POINTS {
            return Err(format!(
                "{}: point count {count} exceeds limit {MAX_DECODE_POINTS}",
                self.name()
            ));
        }
        match self {
            Codec::Raw => raw_decode(bytes, count),
            Codec::Gorilla => gorilla_decode(bytes, count),
            Codec::Delta => delta_decode(bytes, count),
            Codec::Rle => rle_decode(bytes, count),
            Codec::GorillaRle => gorilla_rle_decode(bytes, count),
            Codec::Dolphin => dolphin_decode(bytes, count),
            Codec::Predictor => predictor_decode(bytes, count),
        }
    }
}

fn raw_encode(points: &[Point]) -> Vec<u8> {
    let mut out = Vec::with_capacity(points.len() * 16);
    for p in points {
        out.extend_from_slice(&p.ts.to_le_bytes());
        out.extend_from_slice(&p.value.to_bits().to_le_bytes());
    }
    out
}

fn raw_decode(bytes: &[u8], count: usize) -> Result<Vec<Point>, String> {
    let need = count
        .checked_mul(16)
        .ok_or_else(|| "raw block size overflow".to_string())?;
    if bytes.len() != need {
        return Err(format!(
            "raw block size mismatch: need {need} bytes, got {}",
            bytes.len()
        ));
    }
    let mut out = point_vec(count, "raw")?;
    for i in 0..count {
        let off = i * 16;
        let ts = i64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        let value = f64::from_bits(u64::from_le_bytes(
            bytes[off + 8..off + 16].try_into().unwrap(),
        ));
        out.push(Point { ts, value });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Gorilla
// ---------------------------------------------------------------------------

fn gorilla_encode(points: &[Point]) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(points[0].ts as u64, 64);
    w.write_bits(points[0].value.to_bits(), 64);

    if points.len() >= 2 {
        let mut prev_ts = points[0].ts;
        let mut prev_delta = 0i64;
        let mut prev_bits = points[0].value.to_bits();
        let mut window: Option<(u32, u32)> = None;

        for (i, p) in points.iter().enumerate().skip(1) {
            let delta = p.ts.wrapping_sub(prev_ts);
            if i == 1 {
                // First delta is stored verbatim so all later ones can be
                // delta-of-delta.
                w.write_signed(delta, 64);
            } else {
                let dod = delta.wrapping_sub(prev_delta);
                if dod == 0 {
                    w.write_bit(false);
                } else if (-64..=63).contains(&dod) {
                    w.write_bits(0b10, 2);
                    w.write_signed(dod, 7);
                } else if (-256..=255).contains(&dod) {
                    w.write_bits(0b110, 3);
                    w.write_signed(dod, 9);
                } else if (-2048..=2047).contains(&dod) {
                    w.write_bits(0b1110, 4);
                    w.write_signed(dod, 12);
                } else {
                    w.write_bits(0b1111, 4);
                    w.write_signed(dod, 64);
                }
            }
            prev_delta = delta;
            prev_ts = p.ts;

            let bits = p.value.to_bits();
            encode_xor(&mut w, prev_bits, bits, &mut window);
            prev_bits = bits;
        }
    }

    w.finish()
}

fn encode_xor(w: &mut BitWriter, prev_bits: u64, bits: u64, window: &mut Option<(u32, u32)>) {
    let xor = prev_bits ^ bits;
    if xor == 0 {
        w.write_bit(false);
        return;
    }
    w.write_bit(true);

    let lz = xor.leading_zeros().min(31);
    let tz = xor.trailing_zeros();
    let meaningful = 64 - lz - tz;

    // Reuse costs the old width; a new window costs 11 metadata bits plus
    // the new width. Both paths have the same two tag bits. Prefer reuse on
    // ties, keeping the existing window available for later residuals.
    let reused = window
        .map(|(plz, ptz)| lz >= plz && tz >= ptz && 64 - plz - ptz <= 11 + meaningful)
        .unwrap_or(false);

    if reused {
        let (plz, ptz) = window.unwrap();
        w.write_bit(false);
        w.write_bits(xor >> ptz, 64 - plz - ptz);
    } else {
        w.write_bit(true);
        w.write_bits(lz as u64, 5);
        w.write_bits((meaningful - 1) as u64, 6);
        w.write_bits(xor >> tz, meaningful);
        *window = Some((lz, tz));
    }
}

fn decode_xor(
    r: &mut BitReader,
    prev_bits: u64,
    window: &mut Option<(u32, u32)>,
) -> Result<u64, String> {
    if !r.read_bit() {
        return Ok(prev_bits);
    }
    if !r.read_bit() {
        let (lz, tz) = window.ok_or_else(|| "gorilla: reused empty window".to_string())?;
        let meaningful = 64 - lz - tz;
        let bits = r.read_bits(meaningful) << tz;
        return Ok(prev_bits ^ bits);
    }
    let lz = r.read_bits(5) as u32;
    let meaningful = r.read_bits(6) as u32 + 1;
    if lz + meaningful > 64 {
        return Err("gorilla: invalid window".to_string());
    }
    let tz = 64 - lz - meaningful;
    let bits = r.read_bits(meaningful) << tz;
    *window = Some((lz, tz));
    Ok(prev_bits ^ bits)
}

fn gorilla_decode(bytes: &[u8], count: usize) -> Result<Vec<Point>, String> {
    let mut r = BitReader::new(bytes);
    let ts0 = r.read_bits(64) as i64;
    let v0 = f64::from_bits(r.read_bits(64));
    let mut out = point_vec(count, "gorilla")?;
    out.push(Point { ts: ts0, value: v0 });
    if count == 1 {
        check_bit_padding(&r, bytes.len())?;
        return Ok(out);
    }

    let d0 = r.read_signed(64);
    let mut ts = ts0.wrapping_add(d0);
    let mut prev_delta = d0;
    let mut prev_bits = v0.to_bits();
    let mut window: Option<(u32, u32)> = None;

    for i in 1..count {
        if i >= 2 {
            let dod = if !r.read_bit() {
                0
            } else if !r.read_bit() {
                r.read_signed(7)
            } else if !r.read_bit() {
                r.read_signed(9)
            } else if !r.read_bit() {
                r.read_signed(12)
            } else {
                r.read_signed(64)
            };
            prev_delta = prev_delta.wrapping_add(dod);
            ts = ts.wrapping_add(prev_delta);
        }

        let bits = decode_xor(&mut r, prev_bits, &mut window)?;
        out.push(Point {
            ts,
            value: f64::from_bits(bits),
        });
        prev_bits = bits;
    }

    if r.overrun() {
        return Err("gorilla: truncated block".to_string());
    }
    check_bit_padding(&r, bytes.len())?;
    Ok(out)
}

fn check_bit_padding(r: &BitReader<'_>, byte_len: usize) -> Result<(), String> {
    if r.overrun() {
        return Err("gorilla: truncated block".to_string());
    }
    let total = byte_len
        .checked_mul(8)
        .ok_or_else(|| "gorilla: block size overflow".to_string())?;
    let consumed = r.bits_consumed();
    if consumed > total {
        return Err("gorilla: read past end of block".to_string());
    }
    if total - consumed >= 8 {
        return Err(format!(
            "gorilla: {} trailing bytes after last point",
            (total - consumed) / 8
        ));
    }
    let padding_bits = total - consumed;
    if padding_bits > 0 {
        let mask = (1u8 << padding_bits) - 1;
        if r.buf[byte_len - 1] & mask != 0 {
            return Err("gorilla: nonzero padding bits".to_string());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Gorilla values + GR delta-of-delta timestamp RLE
// ---------------------------------------------------------------------------

/// Timestamps use first timestamp, first delta, then runs of identical
/// delta-of-delta values (the GR idea from Zhou et al., 2026). Fixed-frequency
/// series collapse to one timestamp run.
fn encode_ts_rle(points: &[Point], out: &mut Vec<u8>) {
    put_uvarint(out, zigzag(points[0].ts));

    if points.len() >= 2 {
        // The first delta is stored verbatim; the run loop starts at the
        // third point, so `prev_ts` must already be the second timestamp.
        let mut prev_ts = points[1].ts;
        let mut prev_delta = points[1].ts.wrapping_sub(points[0].ts);
        put_uvarint(out, zigzag(prev_delta));

        let mut runs: Vec<(i64, u64)> = Vec::new();
        for p in points.iter().skip(2) {
            let delta = p.ts.wrapping_sub(prev_ts);
            let dod = delta.wrapping_sub(prev_delta);
            if let Some((last_dod, len)) = runs.last_mut() {
                if *last_dod == dod {
                    *len += 1;
                } else {
                    runs.push((dod, 1));
                }
            } else {
                runs.push((dod, 1));
            }
            prev_delta = delta;
            prev_ts = p.ts;
        }
        put_uvarint(out, runs.len() as u64);
        for (dod, len) in runs {
            put_uvarint(out, zigzag(dod));
            put_uvarint(out, len);
        }
    }
}

fn decode_ts_rle(
    bytes: &[u8],
    pos: &mut usize,
    count: usize,
    codec: &str,
) -> Result<Vec<i64>, String> {
    let ts0 = unzigzag(get_uvarint(bytes, pos)?);
    let mut timestamps = timestamp_vec(count, codec)?;
    timestamps.push(ts0);

    if count >= 2 {
        let mut prev_delta = unzigzag(get_uvarint(bytes, pos)?);
        let mut ts = ts0.wrapping_add(prev_delta);
        timestamps.push(ts);
        let run_count = get_uvarint(bytes, pos)?;
        if run_count > (count - 2) as u64 {
            return Err(format!("{codec}: more timestamp runs than points"));
        }
        for _ in 0..run_count {
            let dod = unzigzag(get_uvarint(bytes, pos)?);
            let len = get_uvarint(bytes, pos)?;
            let remaining = count - timestamps.len();
            if len == 0 || len > remaining as u64 {
                return Err(format!("{codec}: timestamp runs exceed point count"));
            }
            for _ in 0..len {
                prev_delta = prev_delta.wrapping_add(dod);
                ts = ts.wrapping_add(prev_delta);
                timestamps.push(ts);
            }
        }
        if timestamps.len() != count {
            return Err(format!(
                "{codec}: expected {count} timestamps, got {}",
                timestamps.len()
            ));
        }
    }
    Ok(timestamps)
}

fn gorilla_rle_encode(points: &[Point]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_ts_rle(points, &mut out);

    let mut w = BitWriter::new();
    w.write_bits(points[0].value.to_bits(), 64);
    let mut prev_bits = points[0].value.to_bits();
    let mut window: Option<(u32, u32)> = None;
    for p in points.iter().skip(1) {
        let bits = p.value.to_bits();
        encode_xor(&mut w, prev_bits, bits, &mut window);
        prev_bits = bits;
    }
    out.extend_from_slice(&w.finish());
    out
}

fn gorilla_rle_decode(bytes: &[u8], count: usize) -> Result<Vec<Point>, String> {
    let mut pos = 0usize;
    let timestamps = decode_ts_rle(bytes, &mut pos, count, "gorilla-rle")?;
    if pos >= bytes.len() {
        return Err("gorilla-rle: missing value stream".to_string());
    }
    let values = gorilla_values_decode(&bytes[pos..], count)?;
    let mut out = point_vec(count, "gorilla-rle")?;
    for (ts, value) in timestamps.into_iter().zip(values) {
        out.push(Point { ts, value });
    }
    Ok(out)
}

fn gorilla_values_decode(bytes: &[u8], count: usize) -> Result<Vec<f64>, String> {
    let mut r = BitReader::new(bytes);
    let mut out = value_vec(count, "gorilla values")?;
    let mut prev_bits = r.read_bits(64);
    out.push(f64::from_bits(prev_bits));
    let mut window: Option<(u32, u32)> = None;
    for _ in 1..count {
        let bits = decode_xor(&mut r, prev_bits, &mut window)?;
        out.push(f64::from_bits(bits));
        prev_bits = bits;
    }
    check_bit_padding(&r, bytes.len())?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Dolphin-inspired skip encoding
// ---------------------------------------------------------------------------
//
// Dolphin (Chen et al., 2026) observes that oscillating series produce XOR
// residuals with zero leading zeros, but the two high bits flip (01/10/11) and
// are often followed by seven zero bits. It adds a skip path that transmits the
// two-bit prefix once and jumps past the redundant nine-bit prefix region.
//
// This is a self-contained prototype of that idea layered on our Gorilla XOR
// stream; it shares the GR timestamp encoding. A 2-bit value tag selects
// reuse (0), a normal window (10), or a Dolphin skip (11). The skip path stores
// the two prefix bits and the number of bits below the prefix, so the decoder
// never walks the skipped zeros.

/// Fraction of adjacent pairs whose XOR would take the Dolphin skip path.
/// `Block::build` uses this to avoid paying Dolphin's one-bit overhead on
/// blocks where it cannot win. Break-even is roughly 8% because a skip saves
/// ~12 bits while a non-skip window costs one extra bit.
pub(crate) fn dolphin_skip_rate(points: &[Point]) -> f64 {
    if points.len() < 2 {
        return 0.0;
    }
    let mut hits = 0usize;
    let mut prev = points[0].value.to_bits();
    for p in &points[1..] {
        let bits = p.value.to_bits();
        let xor = prev ^ bits;
        let prefix = (xor >> 62) & 0b11;
        let mid = (xor >> 55) & 0x7f;
        if xor != 0 && xor.leading_zeros() == 0 && prefix != 0 && mid == 0 {
            hits += 1;
        }
        prev = bits;
    }
    hits as f64 / (points.len() - 1) as f64
}

fn dolphin_encode(points: &[Point]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_ts_rle(points, &mut out);

    let mut w = BitWriter::new();
    w.write_bits(points[0].value.to_bits(), 64);
    let mut prev_bits = points[0].value.to_bits();
    let mut window: Option<(u32, u32)> = None;
    for p in points.iter().skip(1) {
        let bits = p.value.to_bits();
        dolphin_encode_xor(&mut w, prev_bits, bits, &mut window);
        prev_bits = bits;
    }
    out.extend_from_slice(&w.finish());
    out
}

fn dolphin_encode_xor(
    w: &mut BitWriter,
    prev_bits: u64,
    bits: u64,
    window: &mut Option<(u32, u32)>,
) {
    let xor = prev_bits ^ bits;
    if xor == 0 {
        w.write_bit(false);
        return;
    }
    w.write_bit(true);

    let lz = xor.leading_zeros().min(31);
    let tz = xor.trailing_zeros();
    let meaningful = 64 - lz - tz;

    let reused = window
        .map(|(plz, ptz)| lz >= plz && tz >= ptz)
        .unwrap_or(false);
    if reused {
        let (plz, ptz) = window.unwrap();
        w.write_bit(false);
        w.write_bits(xor >> ptz, 64 - plz - ptz);
        return;
    }

    // Top two bits flipped and bits 2..8 are all zero: skip the 9-bit prefix.
    let prefix = (xor >> 62) & 0b11;
    let mid = (xor >> 55) & 0x7f;
    if lz == 0 && prefix != 0 && mid == 0 {
        let skip_bits = 55u32.saturating_sub(tz);
        w.write_bits(0b11, 2);
        w.write_bits(prefix, 2);
        w.write_bits(skip_bits as u64, 6);
        if skip_bits > 0 {
            let mask = (1u64 << skip_bits) - 1;
            w.write_bits((xor >> tz) & mask, skip_bits);
        }
        *window = Some((9, 55 - skip_bits));
        return;
    }

    w.write_bits(0b10, 2);
    w.write_bits(lz as u64, 5);
    w.write_bits((meaningful - 1) as u64, 6);
    w.write_bits(xor >> tz, meaningful);
    *window = Some((lz, tz));
}

fn dolphin_decode_xor(
    r: &mut BitReader,
    prev_bits: u64,
    window: &mut Option<(u32, u32)>,
) -> Result<u64, String> {
    if !r.read_bit() {
        return Ok(prev_bits);
    }
    if !r.read_bit() {
        let (lz, tz) = window.ok_or_else(|| "dolphin: reused empty window".to_string())?;
        let meaningful = 64 - lz - tz;
        let bits = r.read_bits(meaningful) << tz;
        return Ok(prev_bits ^ bits);
    }
    if !r.read_bit() {
        let lz = r.read_bits(5) as u32;
        let meaningful = r.read_bits(6) as u32 + 1;
        if lz + meaningful > 64 {
            return Err("dolphin: invalid window".to_string());
        }
        let tz = 64 - lz - meaningful;
        let bits = r.read_bits(meaningful) << tz;
        *window = Some((lz, tz));
        return Ok(prev_bits ^ bits);
    }

    let prefix = r.read_bits(2);
    let skip_bits = r.read_bits(6) as u32;
    if skip_bits > 55 {
        return Err("dolphin: invalid skip length".to_string());
    }
    let payload = if skip_bits == 0 {
        0
    } else {
        r.read_bits(skip_bits)
    };
    let tz = 55 - skip_bits;
    let xor = (prefix << 62) | (payload << tz);
    *window = Some((9, tz));
    Ok(prev_bits ^ xor)
}

fn dolphin_decode(bytes: &[u8], count: usize) -> Result<Vec<Point>, String> {
    let mut pos = 0usize;
    let timestamps = decode_ts_rle(bytes, &mut pos, count, "dolphin")?;
    if pos >= bytes.len() {
        return Err("dolphin: missing value stream".to_string());
    }
    let mut r = BitReader::new(&bytes[pos..]);
    let mut values = value_vec(count, "dolphin")?;
    let mut prev_bits = r.read_bits(64);
    values.push(f64::from_bits(prev_bits));
    let mut window: Option<(u32, u32)> = None;
    for _ in 1..count {
        let bits = dolphin_decode_xor(&mut r, prev_bits, &mut window)?;
        values.push(f64::from_bits(bits));
        prev_bits = bits;
    }
    check_bit_padding(&r, bytes.len() - pos)?;

    let mut out = point_vec(count, "dolphin")?;
    for (ts, value) in timestamps.into_iter().zip(values) {
        out.push(Point { ts, value });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// AFC/FPC-inspired FCM + DFCM predictor residuals
// ---------------------------------------------------------------------------
//
// FPC (Burtscher & Ratanaworabhan, 2008) and AFC (Chen et al., 2024) predict
// the next 64-bit pattern with two tables: FCM maps a context to the last value
// seen in it, and DFCM maps a context to the last bit-pattern delta. The
// predictor with the longer zero prefix in its residual wins. We keep that
// idea, select per point, and Gorilla-encode the residual. The four exact AFC
// strategies are not public, so this is an independent predictor prototype.

const PREDICTOR_TABLE_BITS: usize = 12;
const PREDICTOR_TABLE_SIZE: usize = 1 << PREDICTOR_TABLE_BITS;

#[inline]
fn predictor_index(bits: u64) -> usize {
    ((bits >> 52) as usize) & (PREDICTOR_TABLE_SIZE - 1)
}

/// Cheap per-block gate: encode a small prefix with both the predictor and GR.
/// This costs a fraction of one block encode and directly tests whether the
/// predictor is worth trying on the rest of the block.
pub(crate) fn predictor_is_promising(points: &[Point]) -> bool {
    let n = points.len().min(128);
    if n < 8 {
        return false;
    }
    let sample = &points[..n];
    predictor_encode(sample).len() < gorilla_rle_encode(sample).len()
}

fn predictor_encode(points: &[Point]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_ts_rle(points, &mut out);

    let mut w = BitWriter::new();
    let mut prev = points[0].value.to_bits();
    w.write_bits(prev, 64);

    let mut fcm = vec![0u64; PREDICTOR_TABLE_SIZE];
    let mut dfcm = vec![0u64; PREDICTOR_TABLE_SIZE];
    let mut last_delta = 0u64;
    let mut window: Option<(u32, u32)> = None;

    for p in points.iter().skip(1) {
        let actual = p.value.to_bits();

        let fcm_idx = predictor_index(prev);
        let fcm_pred = match fcm[fcm_idx] {
            0 => prev,
            v => v,
        };
        let dfcm_idx = predictor_index(last_delta);
        let dfcm_pred = prev.wrapping_add(dfcm[dfcm_idx]);

        let fcm_lz = (fcm_pred ^ actual).leading_zeros();
        let dfcm_lz = (dfcm_pred ^ actual).leading_zeros();
        let use_dfcm = dfcm_lz > fcm_lz;
        let prediction = if use_dfcm { dfcm_pred } else { fcm_pred };
        w.write_bit(use_dfcm);
        encode_xor(&mut w, prediction, actual, &mut window);

        let delta = actual.wrapping_sub(prev);
        fcm[fcm_idx] = actual;
        dfcm[dfcm_idx] = delta;
        last_delta = delta;
        prev = actual;
    }

    out.extend_from_slice(&w.finish());
    out
}

fn predictor_decode(bytes: &[u8], count: usize) -> Result<Vec<Point>, String> {
    let mut pos = 0usize;
    let timestamps = decode_ts_rle(bytes, &mut pos, count, "predictor")?;
    if pos >= bytes.len() {
        return Err("predictor: missing value stream".to_string());
    }
    let mut r = BitReader::new(&bytes[pos..]);
    let mut prev = r.read_bits(64);
    let mut values = value_vec(count, "predictor")?;
    values.push(f64::from_bits(prev));

    let mut fcm = vec![0u64; PREDICTOR_TABLE_SIZE];
    let mut dfcm = vec![0u64; PREDICTOR_TABLE_SIZE];
    let mut last_delta = 0u64;
    let mut window: Option<(u32, u32)> = None;

    for _ in 1..count {
        let fcm_idx = predictor_index(prev);
        let fcm_pred = match fcm[fcm_idx] {
            0 => prev,
            v => v,
        };
        let dfcm_idx = predictor_index(last_delta);
        let dfcm_pred = prev.wrapping_add(dfcm[dfcm_idx]);

        let use_dfcm = r.read_bit();
        let prediction = if use_dfcm { dfcm_pred } else { fcm_pred };
        let actual = decode_xor(&mut r, prediction, &mut window)?;
        values.push(f64::from_bits(actual));

        let delta = actual.wrapping_sub(prev);
        fcm[fcm_idx] = actual;
        dfcm[dfcm_idx] = delta;
        last_delta = delta;
        prev = actual;
    }
    check_bit_padding(&r, bytes.len() - pos)?;

    let mut out = point_vec(count, "predictor")?;
    for (ts, value) in timestamps.into_iter().zip(values) {
        out.push(Point { ts, value });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Delta + zigzag varints (integral values only)
// ---------------------------------------------------------------------------

const I64_LIMIT: f64 = 9_223_372_036_854_775_808.0; // 2^63

fn delta_encode(points: &[Point]) -> Option<Vec<u8>> {
    let mut ints = Vec::with_capacity(points.len());
    for p in points {
        if !p.value.is_finite()
            || p.value.fract() != 0.0
            || !(p.value >= -I64_LIMIT && p.value < I64_LIMIT)
            // Integer conversion would canonicalize negative zero to +0.
            || p.value.to_bits() == (-0.0f64).to_bits()
        {
            return None;
        }
        ints.push(p.value as i64);
    }

    let mut out = Vec::new();
    put_uvarint(&mut out, zigzag(points[0].ts));
    put_uvarint(&mut out, zigzag(ints[0]));
    for i in 1..points.len() {
        put_uvarint(
            &mut out,
            zigzag(points[i].ts.wrapping_sub(points[i - 1].ts)),
        );
        put_uvarint(&mut out, zigzag(ints[i].wrapping_sub(ints[i - 1])));
    }
    Some(out)
}

fn delta_decode(bytes: &[u8], count: usize) -> Result<Vec<Point>, String> {
    let mut pos = 0usize;
    let mut ts = unzigzag(get_uvarint(bytes, &mut pos)?);
    let mut val = unzigzag(get_uvarint(bytes, &mut pos)?);
    let mut out = point_vec(count, "delta-varint")?;
    out.push(Point {
        ts,
        value: val as f64,
    });
    for _ in 1..count {
        ts = ts.wrapping_add(unzigzag(get_uvarint(bytes, &mut pos)?));
        val = val.wrapping_add(unzigzag(get_uvarint(bytes, &mut pos)?));
        out.push(Point {
            ts,
            value: val as f64,
        });
    }
    if pos != bytes.len() {
        return Err(format!(
            "delta-varint: {} trailing bytes after last point",
            bytes.len() - pos
        ));
    }
    Ok(out)
}

fn zigzag(v: i64) -> u64 {
    ((v as u64) << 1) ^ ((v >> 63) as u64)
}

fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_uvarint(bytes: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut v = 0u64;
    for index in 0..10 {
        if *pos >= bytes.len() {
            return Err("varint truncated".to_string());
        }
        let b = bytes[*pos];
        *pos += 1;
        // A u64 has only one payload bit in its tenth byte. Rejecting excess
        // high bits avoids silent truncation in shifts at bit 63.
        if index == 9 && b > 1 {
            return Err("varint overflow".to_string());
        }
        v |= ((b & 0x7f) as u64) << (index * 7);
        if b & 0x80 == 0 {
            if index > 0 && b == 0 {
                return Err("varint is not minimally encoded".to_string());
            }
            return Ok(v);
        }
    }
    Err("varint overflow".to_string())
}

// ---------------------------------------------------------------------------
// Run length (equal value at a fixed step)
// ---------------------------------------------------------------------------

fn rle_encode(points: &[Point]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut runs: Vec<(i64, i64, u64, u64)> = Vec::new(); // start, step, len, bits

    let mut i = 0usize;
    while i < points.len() {
        let start = points[i].ts;
        let bits = points[i].value.to_bits();
        let step = if i + 1 < points.len() {
            points[i + 1].ts.wrapping_sub(points[i].ts)
        } else {
            0
        };
        let mut j = i + 1;
        while j < points.len()
            && points[j].value.to_bits() == bits
            && points[j].ts.wrapping_sub(points[j - 1].ts) == step
        {
            j += 1;
        }
        runs.push((start, step, (j - i) as u64, bits));
        i = j;
    }

    put_uvarint(&mut out, runs.len() as u64);
    let mut prev_start = 0i64;
    for (start, step, len, bits) in runs {
        put_uvarint(&mut out, zigzag(start.wrapping_sub(prev_start)));
        prev_start = start;
        put_uvarint(&mut out, zigzag(step));
        put_uvarint(&mut out, len);
        out.extend_from_slice(&bits.to_le_bytes());
    }
    out
}

fn rle_decode(bytes: &[u8], count: usize) -> Result<Vec<Point>, String> {
    let mut pos = 0usize;
    let run_count = get_uvarint(bytes, &mut pos)?;
    if run_count > count as u64 {
        return Err("rle: more runs than points".to_string());
    }
    let mut out = point_vec(count, "rle")?;
    let mut prev_start = 0i64;
    for _ in 0..run_count {
        let start = prev_start.wrapping_add(unzigzag(get_uvarint(bytes, &mut pos)?));
        prev_start = start;
        let step = unzigzag(get_uvarint(bytes, &mut pos)?);
        let len = get_uvarint(bytes, &mut pos)?;
        let value_end = pos
            .checked_add(8)
            .ok_or_else(|| "rle: value offset overflow".to_string())?;
        if value_end > bytes.len() {
            return Err("rle: truncated value".to_string());
        }
        let bits = u64::from_le_bytes(bytes[pos..value_end].try_into().unwrap());
        pos = value_end;
        let value = f64::from_bits(bits);
        let remaining = count
            .checked_sub(out.len())
            .ok_or_else(|| "rle: decoded points exceed point count".to_string())?;
        if len > u64::try_from(remaining).unwrap_or(u64::MAX) {
            return Err("rle: runs exceed point count".to_string());
        }
        let run_len = usize::try_from(len)
            .map_err(|_| "rle: run length exceeds platform limits".to_string())?;
        for k in 0..run_len {
            let ts = start.wrapping_add((k as i64).wrapping_mul(step));
            out.push(Point { ts, value });
        }
    }
    if out.len() != count {
        return Err(format!("rle: expected {count} points, got {}", out.len()));
    }
    if pos != bytes.len() {
        return Err(format!(
            "rle: {} trailing bytes after last run",
            bytes.len() - pos
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Point> {
        let mut pts = Vec::new();
        let mut ts = 1_700_000_000_000i64;
        let mut v = 20.0f64;
        for i in 0..1500 {
            ts += 1000 + (i % 7) as i64;
            v += ((i % 11) as f64 - 5.0) * 0.25;
            if i % 97 == 0 {
                v = 99.5;
            }
            pts.push(Point { ts, value: v });
        }
        pts
    }

    #[test]
    fn xor_window_chooses_cheaper_encoding_and_preserves_ties() {
        let mut w = BitWriter::new();
        let mut window = Some((0, 0));
        encode_xor(&mut w, 0, 1, &mut window);
        assert_eq!(window, Some((31, 0)));
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert!(r.read_bit());
        assert!(r.read_bit()); // A new narrow window replaces the 64-bit one.
        let mut decode_window = Some((0, 0));
        let mut r = BitReader::new(&bytes);
        assert_eq!(decode_xor(&mut r, 0, &mut decode_window).unwrap(), 1);
        check_bit_padding(&r, bytes.len()).unwrap();
        assert!(check_bit_padding(&r, bytes.len() - 1).is_err());

        let mut w = BitWriter::new();
        let mut window = Some((31, 0));
        encode_xor(&mut w, 0, 1 << 11, &mut window);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert!(r.read_bit());
        assert!(!r.read_bit()); // Reuse and new window both cost 33 bits.
        assert_eq!(window, Some((31, 0)));
    }

    #[test]
    fn narrowing_then_widening_preserves_exact_value_bits() {
        let bits = [
            0,
            0x8000_0000_0000_0001,
            0x8000_0000_0000_0000,
            0x7ff8_0000_0000_0042,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0,
        ];
        let points: Vec<_> = bits
            .into_iter()
            .enumerate()
            .map(|(i, bits)| Point {
                ts: i as i64,
                value: f64::from_bits(bits),
            })
            .collect();
        for codec in [Codec::Gorilla, Codec::GorillaRle, Codec::Predictor] {
            let bytes = codec.encode(&points).unwrap();
            let decoded = codec.decode(&bytes, points.len()).unwrap();
            for (actual, expected) in decoded.iter().zip(&points) {
                assert_eq!(
                    (actual.ts, actual.value.to_bits()),
                    (expected.ts, expected.value.to_bits())
                );
            }
            assert!(codec
                .decode(&bytes[..bytes.len() - 1], points.len())
                .is_err());
        }
    }

    #[test]
    fn gorilla_roundtrip() {
        let pts = sample();
        let bytes = Codec::Gorilla.encode(&pts).unwrap();
        let back = Codec::Gorilla.decode(&bytes, pts.len()).unwrap();
        assert_eq!(back, pts);
    }

    #[test]
    fn gorilla_roundtrip_specials() {
        let pts = [
            Point { ts: -5, value: 0.0 },
            Point {
                ts: -4,
                value: -0.0,
            },
            Point {
                ts: -3,
                value: f64::INFINITY,
            },
            Point {
                ts: -2,
                value: f64::NEG_INFINITY,
            },
            Point {
                ts: -1,
                value: f64::NAN,
            },
            Point {
                ts: 0,
                value: f64::MIN_POSITIVE,
            },
            Point {
                ts: 1,
                value: 1.0000000000000002,
            },
        ];
        let bytes = Codec::Gorilla.encode(&pts).unwrap();
        let back = Codec::Gorilla.decode(&bytes, pts.len()).unwrap();
        for (a, b) in pts.iter().zip(back.iter()) {
            assert_eq!(a.ts, b.ts);
            assert_eq!(a.value.to_bits(), b.value.to_bits());
        }
    }

    #[test]
    fn delta_roundtrip() {
        let pts: Vec<Point> = (0..1000)
            .map(|i| Point {
                ts: 1_000_000 + i * 500,
                value: (i * 3) as f64,
            })
            .collect();
        let bytes = Codec::Delta.encode(&pts).unwrap();
        let back = Codec::Delta.decode(&bytes, pts.len()).unwrap();
        assert_eq!(back, pts);
    }

    #[test]
    fn delta_rejects_fractional() {
        let pts = vec![Point { ts: 1, value: 1.5 }];
        assert!(Codec::Delta.encode(&pts).is_none());
    }

    #[test]
    fn rle_roundtrip() {
        let pts: Vec<Point> = (0..500)
            .map(|i| Point {
                ts: 1000 + i * 10,
                value: if i < 300 { 7.0 } else { 8.0 },
            })
            .collect();
        let bytes = Codec::Rle.encode(&pts).unwrap();
        let back = Codec::Rle.decode(&bytes, pts.len()).unwrap();
        assert_eq!(back, pts);
    }

    #[test]
    fn raw_roundtrip() {
        let pts = sample();
        let bytes = Codec::Raw.encode(&pts).unwrap();
        let back = Codec::Raw.decode(&bytes, pts.len()).unwrap();
        assert_eq!(back, pts);
    }

    #[test]
    fn rle_compresses_constants_more_than_gorilla() {
        let pts: Vec<Point> = (0..1000)
            .map(|i| Point {
                ts: i * 1000,
                value: 42.0,
            })
            .collect();
        let gorilla = Codec::Gorilla.encode(&pts).unwrap();
        let rle = Codec::Rle.encode(&pts).unwrap();
        assert!(rle.len() < gorilla.len());
        assert!(rle.len() < pts.len() * 16);
    }

    #[test]
    fn gorilla_roundtrip_linear() {
        let pts: Vec<Point> = (0..1024)
            .map(|i| Point {
                ts: 1_700_000_000_000 + i * 1000,
                value: i as f64 * 2.0,
            })
            .collect();
        let bytes = Codec::Gorilla.encode(&pts).unwrap();
        let back = Codec::Gorilla.decode(&bytes, pts.len()).unwrap();
        assert_eq!(back, pts);
    }

    #[test]
    fn gorilla_rle_roundtrip_fixed_and_irregular() {
        let fixed: Vec<Point> = (0..1024)
            .map(|i| Point {
                ts: 1_700_000_000_000 + i * 1000,
                value: i as f64 * 2.0,
            })
            .collect();
        let bytes = Codec::GorillaRle.encode(&fixed).unwrap();
        let back = Codec::GorillaRle.decode(&bytes, fixed.len()).unwrap();
        assert_eq!(back, fixed);
        assert!(
            bytes.len() < Codec::Gorilla.encode(&fixed).unwrap().len(),
            "GR should beat Gorilla on fixed-step timestamps"
        );

        let irregular = sample();
        let bytes = Codec::GorillaRle.encode(&irregular).unwrap();
        let back = Codec::GorillaRle.decode(&bytes, irregular.len()).unwrap();
        assert_eq!(back, irregular);
    }

    #[test]
    fn gorilla_rle_roundtrip_specials_and_short() {
        let pts = [
            Point { ts: -5, value: 0.0 },
            Point {
                ts: -5,
                value: f64::NAN,
            },
            Point {
                ts: i64::MAX,
                value: f64::NEG_INFINITY,
            },
        ];
        for n in 1..=pts.len() {
            let bytes = Codec::GorillaRle.encode(&pts[..n]).unwrap();
            let back = Codec::GorillaRle.decode(&bytes, n).unwrap();
            assert_eq!(back.len(), n);
            for (a, b) in pts[..n].iter().zip(back.iter()) {
                assert_eq!(a.ts, b.ts);
                assert_eq!(a.value.to_bits(), b.value.to_bits());
            }
        }
    }

    #[test]
    fn varint_and_zigzag_helpers() {
        let mut v = Vec::new();
        for x in [0u64, 1, 127, 128, 300, u64::MAX] {
            v.clear();
            put_uvarint(&mut v, x);
            let mut p = 0;
            assert_eq!(get_uvarint(&v, &mut p).unwrap(), x);
            assert_eq!(p, v.len());
        }
        for x in [i64::MIN, -1000, -1, 0, 1, 1000, i64::MAX] {
            assert_eq!(unzigzag(zigzag(x)), x);
        }
    }

    #[test]
    fn varints_reject_overflow_and_noncanonical_forms() {
        for bad in [
            vec![0xff; 9].into_iter().chain([0x02]).collect::<Vec<_>>(),
            vec![0xff; 10],
            vec![0x80, 0x00],
        ] {
            let mut pos = 0;
            assert!(get_uvarint(&bad, &mut pos).is_err(), "accepted {bad:?}");
        }
        let mut pos = 0;
        assert_eq!(
            get_uvarint(
                &[0xff; 9].into_iter().chain([1]).collect::<Vec<_>>(),
                &mut pos
            )
            .unwrap(),
            u64::MAX
        );
        assert!(Codec::Delta
            .decode(
                &[0xff; 9].into_iter().chain([2, 0, 0]).collect::<Vec<_>>(),
                1
            )
            .is_err());
        assert!(Codec::Delta.decode(&[0x80, 0, 0], 1).is_err());
    }

    #[test]
    fn public_decode_rejects_unbounded_counts_and_nonempty_empty_frames() {
        assert!(Codec::Raw.decode(&[], MAX_DECODE_POINTS + 1).is_err());
        assert!(Codec::Raw.decode(&[0], 0).is_err());
    }

    #[test]
    fn gorilla_rejects_nonzero_padding() {
        let points = [Point { ts: 10, value: 1.0 }, Point { ts: 11, value: 1.0 }];
        let mut bytes = Codec::Gorilla.encode(&points).unwrap();
        assert_ne!(bytes.last().unwrap() & 1, 1);
        *bytes.last_mut().unwrap() |= 1;
        assert!(Codec::Gorilla.decode(&bytes, points.len()).is_err());
    }

    /// The source points are the independent oracle: codec round trips must
    /// preserve timestamps and all 64 value bits, including NaN payloads and
    /// signed zero. The fixed corpus is intentionally small and deterministic.
    #[test]
    fn bounded_codec_oracle_preserves_every_float_bit() {
        let value_bits = [
            0x0000_0000_0000_0000,
            0x8000_0000_0000_0000,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff8_0000_0000_0042,
            0x0000_0000_0000_0001,
            0x3ff0_0000_0000_0001,
            0xbff0_0000_0000_0000,
        ];
        let points: Vec<Point> = value_bits
            .iter()
            .enumerate()
            .map(|(i, bits)| Point {
                ts: i as i64 - 4,
                value: f64::from_bits(*bits),
            })
            .collect();

        for codec in Codec::ALL {
            if let Some(encoded) = codec.encode(&points) {
                let decoded = codec.decode(&encoded, points.len()).unwrap();
                assert_eq!(decoded.len(), points.len(), "{codec:?}");
                for (expected, actual) in points.iter().zip(decoded.iter()) {
                    assert_eq!(actual.ts, expected.ts, "{codec:?} timestamp");
                    assert_eq!(
                        actual.value.to_bits(),
                        expected.value.to_bits(),
                        "{codec:?} value"
                    );
                }
            }
        }
    }

    /// Deterministic xorshift so the randomized test is reproducible and
    /// dependency-free.
    fn xorshift(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    #[test]
    fn randomized_roundtrips_for_every_codec() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for trial in 0..64 {
            let n = 1 + (xorshift(&mut state) as usize % 300);
            let mut ts = (xorshift(&mut state) as i64) % 10_000;
            let mut points = Vec::with_capacity(n);
            for _ in 0..n {
                // Mix fixed steps, irregular steps, and occasional equal values
                // so every codec path (runs, XOR windows, varints) is exercised.
                let step = match xorshift(&mut state) % 4 {
                    0 => 1,
                    1 => 1000,
                    _ => 1 + (xorshift(&mut state) as i64 % 50),
                };
                ts = ts.saturating_add(step);
                let value = match xorshift(&mut state) % 8 {
                    0 => 0.0,
                    1 => -0.0,
                    2 => f64::NAN,
                    3 => f64::INFINITY,
                    4 => f64::NEG_INFINITY,
                    5 => (xorshift(&mut state) as f64) / 7.0,
                    _ => (xorshift(&mut state) % 1000) as f64,
                };
                points.push(Point { ts, value });
            }

            for codec in Codec::ALL {
                let Some(bytes) = codec.encode(&points) else {
                    continue;
                };
                let back = codec
                    .decode(&bytes, points.len())
                    .unwrap_or_else(|e| panic!("{codec:?} trial {trial} failed: {e}"));
                assert_eq!(back.len(), points.len(), "{codec:?} trial {trial}");
                for (a, b) in points.iter().zip(back.iter()) {
                    assert_eq!(a.ts, b.ts, "{codec:?} trial {trial} timestamp");
                    assert_eq!(
                        a.value.to_bits(),
                        b.value.to_bits(),
                        "{codec:?} trial {trial} value"
                    );
                }
            }
        }
    }

    #[test]
    fn dolphin_roundtrip_and_win_on_oscillating_data() {
        let pts = crate::gen::generate(
            crate::gen::Pattern::Oscillating,
            1024,
            1_700_000_000_000,
            1000,
        );
        let dolphin = Codec::Dolphin.encode(&pts).unwrap();
        let gorilla = Codec::GorillaRle.encode(&pts).unwrap();
        assert!(dolphin.len() < gorilla.len(), "dolphin should win");
        let back = Codec::Dolphin.decode(&dolphin, pts.len()).unwrap();
        assert_eq!(back.len(), pts.len());
        for (a, b) in pts.iter().zip(back.iter()) {
            assert_eq!(a.ts, b.ts);
            assert_eq!(a.value.to_bits(), b.value.to_bits());
        }
    }

    #[test]
    fn predictor_wins_on_predictable_counter_and_roundtrips() {
        let pts = crate::gen::generate(crate::gen::Pattern::Counter, 1024, 1_700_000_000_000, 1000);
        let predictor = Codec::Predictor.encode(&pts).unwrap();
        let gorilla = Codec::GorillaRle.encode(&pts).unwrap();
        assert!(
            predictor.len() * 2 < gorilla.len(),
            "predictor should win big"
        );
        let back = Codec::Predictor.decode(&predictor, pts.len()).unwrap();
        assert_eq!(back.len(), pts.len());
        for (a, b) in pts.iter().zip(back.iter()) {
            assert_eq!(a.ts, b.ts);
            assert_eq!(a.value.to_bits(), b.value.to_bits());
        }
    }

    #[test]
    fn predictor_gate_recognizes_predictable_data() {
        let counter =
            crate::gen::generate(crate::gen::Pattern::Counter, 1024, 1_700_000_000_000, 1000);
        assert!(predictor_is_promising(&counter));
        let noise = crate::gen::generate(crate::gen::Pattern::Noise, 1024, 1_700_000_000_000, 1000);
        assert!(!predictor_is_promising(&noise));
    }

    #[test]
    fn codec_ids_are_stable() {
        // IDs are persisted in block headers; never renumber an existing codec.
        for (id, codec) in [
            (0, Codec::Raw),
            (1, Codec::Gorilla),
            (2, Codec::Delta),
            (3, Codec::Rle),
            (4, Codec::GorillaRle),
            (5, Codec::Dolphin),
            (6, Codec::Predictor),
        ] {
            assert_eq!(Codec::from_u8(id), Some(codec));
            assert_eq!(codec as u8, id);
        }
        assert_eq!(Codec::from_u8(7), None);
    }
}
