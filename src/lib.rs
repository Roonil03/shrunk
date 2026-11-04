//! shrunk - a small time series database that keeps data compressed and
//! answers queries from compressed blocks.
//!
//! ```no_run
//! use shrunk::{Db, Query};
//! let mut db = Db::new();
//! for i in 0..10_000 {
//!     db.write("cpu", 1_700_000_000_000 + i * 1000, 40.0 + (i % 10) as f64).unwrap();
//! }
//! let result = db.query(&Query::new("cpu", 1_700_000_000_000, 1_700_000_009_000)).unwrap();
//! assert_eq!(result.points.len(), 10);
//! ```
//!
//! Data is grouped into blocks (default 1024 points). Each block is encoded
//! with Gorilla, delta-varint, RLE, and raw, and the smallest encoding wins.
//! Blocks carry time bounds, value bounds, a sum, and a NaN flag so reads and
//! aggregates can skip work.

pub mod bits;
pub mod block;
pub mod codec;
mod crc;
pub mod db;
pub mod gen;

pub use block::Block;
pub use codec::Codec;
pub use db::{
    Agg, AggResult, CodecStat, Db, Error, Predicate, Query, QueryResult, QueryStats, Result,
    SeriesSummary, FORMAT_MAGIC, FORMAT_VERSION,
};

/// One sample: a millisecond timestamp and a value.
#[derive(Clone, Copy, Debug)]
pub struct Point {
    pub ts: i64,
    pub value: f64,
}

impl Point {
    pub fn new(ts: i64, value: f64) -> Self {
        Self { ts, value }
    }
}

/// Bitwise equality, so NaN values compare equal when reading back a block.
impl PartialEq for Point {
    fn eq(&self, other: &Self) -> bool {
        self.ts == other.ts && self.value.to_bits() == other.value.to_bits()
    }
}
