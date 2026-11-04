use crate::codec::Codec;
use crate::Point;

/// Bytes of block header written to disk, excluding the payload:
/// codec(1) + flags(1) + count(4) + t_min(8) + t_max(8) + v_min(8) +
/// v_max(8) + sum(8) + payload_len(4) + crc(4).
pub const BLOCK_HEADER_BYTES: usize = 54;

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
    pub payload: Vec<u8>,
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
        for codec in Codec::CANDIDATES {
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
            sum: if all_finite { Some(sum) } else { None },
            payload: best,
        }
    }

    pub fn decode(&self) -> Result<Vec<Point>, String> {
        self.codec.decode(&self.payload, self.count as usize)
    }

    pub fn stored_bytes(&self) -> usize {
        BLOCK_HEADER_BYTES + self.payload.len()
    }

    pub fn raw_bytes(&self) -> u64 {
        self.count as u64 * 16
    }
}
