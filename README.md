# shrunk

A small time series database that keeps points compressed and avoids decoding
blocks a query does not need.

The idea: group points into fixed-size blocks, compress each block with the
codec that wins on that block, and store enough per-block metadata (time range,
value range, count, sum) that reads and aggregates can skip work. Timestamps and
values are not stored as 16 raw bytes per point unless a block is truly
incompressible.

Written in Rust with no dependencies.

## Try it

```sh
cargo build --release
./target/release/shrunk demo
```

Output from this machine (Apple M3 Pro, macOS 27.0.1, release build, synthetic
data):

```text
seeded 3 series x 200000 points with Counter in 30 ms; file 1.7 MiB

series                   points   blocks      stored         raw    ratio  bits/pt  codecs
cpu.load                 200000      196     1.4 MiB     3.1 MiB     2.2x    57.07  gorilla 196b/1426685B
requests.total           200000      196   310.3 KiB     3.1 MiB    10.1x    12.71  gorilla 196b/317780B
thermostat.setpoint      200000      196    14.0 KiB     3.1 MiB   223.7x     0.57  rle 196b/14308B
total                    600000              1.7 MiB     9.2 MiB     5.5x

avg(cpu.load) over all points = 45.1299 (196 of 196 blocks read from metadata)
query cpu.load over 300 s: 300 points returned
blocks: 196 total, 2 visited, 2 decoded, 194 skipped (194 time, 0 value)
read: 2048 points from 14583 bytes of compressed payload
query whole series with value > 90 (limit 10): 10 spike points returned
blocks: 196 total, 88 visited, 10 decoded, 78 skipped (0 time, 78 value), 108 left unread after limit
read: 10240 points from 72011 bytes of compressed payload
count over the same 300 s = Some(300.0) (0 of 196 blocks answered from metadata)
```

The count query covers 300 inclusive timestamps.

One million noisy gauge points (one run; timings vary by a few percent):

```text
ingest         61.3 ms  (16.31M points/s)
save           65.2 ms  (6.8 MiB on disk)
open           33.4 ms
scan all       32.8 ms  (1000000 points returned)
avg all         0.0 ms  (977 blocks answered from metadata)
filtered        0.1 ms  (2 points, 975 blocks skipped of 977)
ratio 2.2x (57 bits/point)
```

The noisy gauge is the hard case: Gorilla XOR compression gets about 2.2x on
f64 sensor values. Counters and constants do much better because they match the
delta and RLE codecs. Numbers vary with the data; the benchmark command is
repeatable.

## How it works

### Blocks and codec selection

Each series is an ordered list of immutable blocks, 1024 points by default.
When a block fills, `shrunk` encodes it with every applicable codec and keeps
the smallest result:

| codec | timestamps | values | good for |
|---|---|---|---|
| `gorilla` | delta-of-delta, variable width | XOR of f64 bit patterns with leading/trailing zero windows | gauges, noisy floats |
| `delta-varint` | zigzag varint delta | zigzag varint delta, integral values only | counters, IDs, integer metrics |
| `rle` | start, fixed step, run length | one f64 per run | constants, flat sensors |
| `raw` | 16 bytes per point | 8-byte timestamp plus 8-byte f64 | incompressible fallback |

Gorilla follows the format from Facebook's Gorilla paper: the first timestamp
is 64 bits, the first delta is 64 bits, later timestamps use delta-of-delta
prefixes (`0`, `10`, `110`, `1110`, `1111`), and values reuse a previous XOR
window when the new XOR fits inside it.

Every block header also stores:

- `t_min`, `t_max`: time bounds
- `v_min`, `v_max`: value bounds
- `count`: points in the block
- `sum`: total when all values are finite
- `has_nan`: disables value and min/max shortcuts when NaN is present

The header costs 54 bytes plus the payload. That is negligible for a 1024-point
block and noticeable for a three-point series.

### Query path

`query` and `aggregate` do this per block:

1. Binary search to the first block whose `t_max` is at or after the start.
2. Skip blocks whose `t_min` is past the end.
3. If a value predicate exists, skip blocks whose `[v_min, v_max]` cannot
   satisfy it.
4. Decode only the blocks that survive.
5. For aggregates with no value predicate, answer a fully covered block from
   the header instead of decoding anything: count from `count`, sum and average
   from `sum`, min and max from `v_min`/`v_max`.

That last step is why `avg` over one million points takes under a millisecond.
It uses the stored per-block totals, not sampling.

`--explain` prints how much was skipped:

```text
blocks: 196 total, 2 visited, 2 decoded, 194 skipped (194 time, 0 value)
read: 2048 points from 14583 bytes of compressed payload
```

The fields are total blocks, blocks entered, blocks decoded, blocks skipped by
time or value bounds, and points read. A `--limit` that stops early reports the
remaining blocks as left unread instead of counting them as time skips.

This is not a general compressed-domain execution engine. Arbitrary arithmetic
on encoded bits is not supported. The compression is queryable through block
pruning and stored aggregates.

## CLI

```text
shrunk seed <db> --series NAME [--pattern cpu|counter|constant|noise|saw]
               [--points N] [--interval MS] [--start MS] [--block-size N]

shrunk ingest <db> --series NAME --file PATH

shrunk query <db> --series NAME [--from MS] [--to MS]
              [--agg count|sum|min|max|avg]
              [--gt X|--ge X|--lt X|--le X|--eq X|--ne X]
              [--limit N] [--csv] [--explain]

shrunk stats <db>
shrunk bench [--points N] [--pattern NAME] [--block-size N]
shrunk demo [--points N]
```

Timestamps are milliseconds and time bounds are inclusive. `ingest` accepts
`timestamp_ms,value` lines and skips a header row.

Example:

```sh
shrunk seed /tmp/cpu.db --series cpu --points 100000 --pattern cpu
shrunk stats /tmp/cpu.db
shrunk query /tmp/cpu.db --series cpu --from 1700000000000 --to 1700000600000 --agg avg --explain
shrunk query /tmp/cpu.db --series cpu --from 1700000000000 --to 1700000600000 --gt 60 --limit 5
```

## Library

```rust
use shrunk::{Agg, Db, Predicate, Query};

let mut db = Db::with_block_size(1024);
for i in 0..100_000 {
    db.write("cpu", 1_700_000_000_000 + i * 1000, 40.0 + (i % 17) as f64)
        .unwrap();
}

let scan = db
    .query(&Query::new("cpu", 1_700_000_000_000, 1_700_000_060_000))
    .unwrap();
println!(
    "{} points, {} blocks decoded",
    scan.points.len(),
    scan.stats.blocks_decoded
);

let hot = db
    .query(
        &Query::new("cpu", 1_700_000_000_000, 1_700_000_060_000)
            .filter(Predicate::Gt(50.0)),
    )
    .unwrap();
println!("{} hot points", hot.points.len());

let avg = db.aggregate("cpu", 0, i64::MAX, Agg::Avg, None).unwrap();
println!("avg = {:?}", avg.value);
```

Writes must be in non-decreasing timestamp order per series. A point older than
the last one is rejected with `Error::OutOfOrder`.

## On-disk format

A database is one file. `save` writes a temp file, fsyncs it, renames it, then
fsyncs the directory. Each block payload has a CRC32; loading rejects a checksum
mismatch.

```text
"SHRUNK01" (8 bytes)
u32 block_size
u32 series_count

per series:
  u32 name_len
  name bytes (UTF-8)
  u32 block_count

  per block:
    u8  codec           0 raw, 1 gorilla, 2 delta-varint, 3 rle
    u8  flags           bit 0 sum present, bit 1 has NaN
    u32 count
    i64 t_min
    i64 t_max
    f64 v_min
    f64 v_max
    f64 sum
    u32 payload_len
    payload
    u32 crc32(payload)
```

`save` flushes pending points first, so reopening produces the same block
layout. Pending points are not persisted until save.

## Tests

```sh
cargo test
```

24 tests: 17 unit, 6 integration, 1 doc test. All pass.

They cover bit-level IO, round trips for all four codecs including NaN,
infinities, -0.0, and negative timestamps, codec selection, time and value
block skipping, aggregate results checked against a naive fold, limits,
persistence, append after reopen, and CRC rejection of a corrupted payload.

## Layout

```text
src/bits.rs      bit writer/reader
src/codec.rs     gorilla, delta-varint, rle, raw
src/block.rs     block construction, metadata, codec selection
src/db.rs        series, writes, queries, aggregates, file format
src/gen.rs       deterministic synthetic data
src/main.rs      CLI
tests/           integration tests
```

## Limits

These are deliberate for a small project:

- One writer at a time. No WAL, no concurrent writers, no transactions.
- `save` rewrites the whole file. Fine for hundreds of megabytes, not for
  terabytes.
- The database loads into memory on open.
- No deletes, retention windows, downsampling, or rollups.
- No multi-series joins or expression language. One series per query, one
  comparison per value filter.
- Gorilla's advantage depends on data. Noisy f64 gauges get about 2x. Integer
  counters and flat signals get 10x to 200x.
- A 54-byte block header means very small series can occupy more space than raw
  points. Use a larger block size for many tiny series.
- Aggregates ignore NaN in sum, avg, min, and max. Count counts every point.
