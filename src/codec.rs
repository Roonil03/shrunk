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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Codec {
    Raw = 0,
    Gorilla = 1,
    Delta = 2,
    Rle = 3,
}

impl Codec {
    /// Order matters: on a size tie the earlier codec wins.
    pub const CANDIDATES: [Codec; 4] = [Codec::Gorilla, Codec::Delta, Codec::Rle, Codec::Raw];

    pub fn from_u8(v: u8) -> Option<Codec> {
        match v {
            0 => Some(Codec::Raw),
            1 => Some(Codec::Gorilla),
            2 => Some(Codec::Delta),
            3 => Some(Codec::Rle),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::Raw => "raw",
            Codec::Gorilla => "gorilla",
            Codec::Delta => "delta-varint",
            Codec::Rle => "rle",
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
        }
    }

    pub fn decode(self, bytes: &[u8], count: usize) -> Result<Vec<Point>, String> {
        if count == 0 {
            return Ok(Vec::new());
        }
        match self {
            Codec::Raw => raw_decode(bytes, count),
            Codec::Gorilla => gorilla_decode(bytes, count),
            Codec::Delta => delta_decode(bytes, count),
            Codec::Rle => rle_decode(bytes, count),
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
    if bytes.len() < need {
        return Err(format!(
            "raw block truncated: need {need} bytes, got {}",
            bytes.len()
        ));
    }
    let mut out = Vec::with_capacity(count);
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

    let reused = window
        .map(|(plz, ptz)| lz >= plz && tz >= ptz)
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
    let mut out = Vec::with_capacity(count);
    out.push(Point { ts: ts0, value: v0 });
    if count == 1 {
        if r.overrun() {
            return Err("gorilla: truncated block".to_string());
        }
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
    let mut out = Vec::with_capacity(count);
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
    let mut shift = 0u32;
    loop {
        if *pos >= bytes.len() {
            return Err("varint truncated".to_string());
        }
        let b = bytes[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift >= 64 {
            return Err("varint overflow".to_string());
        }
    }
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
    let mut out = Vec::with_capacity(count);
    let mut prev_start = 0i64;
    for _ in 0..run_count {
        let start = prev_start.wrapping_add(unzigzag(get_uvarint(bytes, &mut pos)?));
        prev_start = start;
        let step = unzigzag(get_uvarint(bytes, &mut pos)?);
        let len = get_uvarint(bytes, &mut pos)?;
        if pos + 8 > bytes.len() {
            return Err("rle: truncated value".to_string());
        }
        let bits = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let value = f64::from_bits(bits);
        if out.len() + (len as usize) > count {
            return Err("rle: runs exceed point count".to_string());
        }
        for k in 0..len {
            let ts = start.wrapping_add((k as i64).wrapping_mul(step));
            out.push(Point { ts, value });
        }
    }
    if out.len() != count {
        return Err(format!("rle: expected {count} points, got {}", out.len()));
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
    fn gorilla_roundtrip() {
        let pts = sample();
        let bytes = Codec::Gorilla.encode(&pts).unwrap();
        let back = Codec::Gorilla.decode(&bytes, pts.len()).unwrap();
        assert_eq!(back, pts);
    }

    #[test]
    fn gorilla_roundtrip_specials() {
        let pts = vec![
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
}
