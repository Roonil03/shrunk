# shrunk

shrunk stores time-stamped numbers in less space and answers questions without
unpacking every reading.

## The problem, without the jargon

Picture this. A small greenhouse has 100 temperature and humidity sensors. Each
sensor sends a reading every 10 seconds. That is 864,000 readings a day, or 315
million readings a year. Every reading is just a timestamp and a number.

Stored the usual way, that is about 5 GB a year for one greenhouse. Two
greenhouses over five years is 50 GB of tiny numbers. Backups get slower, disks
fill up, and a simple question like "what was the temperature last Tuesday at
2:15?" makes the software walk through millions of readings to find a few
hundred.

The readings themselves are small. The cost of keeping and searching them is
not.

shrunk changes how the readings are stored:

- Readings are grouped into blocks. The default is 1024 readings, about 3 hours
  of data at one reading every 10 seconds.
- Each block is compressed with whichever codec makes that block smallest. A
  flat thermostat uses a different codec than a noisy sensor or an oscillating
  waveform.
- Each block gets a label: its time range, its lowest and highest value, how
  many readings it holds, and their sum.
- Queries use labels to skip blocks. Totals come from the labels, so a question
  like "what was the average?" does not need to unpack every reading.

On 600,000 synthetic readings from three sensors:

```text
9.2 MiB of raw readings  ->  1.4 MiB stored  (6.5x smaller)

noisy gauge       2.3x smaller
counter          48.7x smaller
constant value  212.0x smaller
```

A 5-minute query read 2 blocks out of 196. The other 194 stayed compressed.

## How a query avoids work

Think of each block as a sealed box of readings with a label on the lid:

```text
box 42
time:   14:00 to 16:49
count:  1024 readings
lowest: 18.2
highest: 24.9
sum:    22,431.7
```

Now ask "did the temperature ever go above 23?"

Any box whose label says `highest: 22.1` cannot contain a match. Leave it
sealed. Only open boxes whose label says the highest value is at least 23.

Ask "what was the average?" Add the sums printed on the labels and divide by
the counts. No box needs to be opened.

That is the whole trick. The compression makes the file smaller. The labels
make the searches smaller.

## Why not just zip the file?

Zip compresses a whole file into one stream. To read one page, you have to
unpack the stream.

shrunk compresses small independent blocks and keeps a label on each one. It
can unpack one block without touching the other 195. A zip file answers "what is
on page 400?" by unpacking everything up to page 400. shrunk answers by reading
the label on the box.

## What shrunk is not

- It is not a cloud service. It is one file and one local process.
- It is not a replacement for Prometheus, InfluxDB, or TimescaleDB. There is no
  replication, clustering, retention policy, or SQL.
- It is not lossless magic for every data set. Noisy floating point sensors get
  about 2x. Integer counters and flat signals get 10x to 200x.
- It does not load every payload on open. `open` reads metadata only and reads a
  block's compressed bytes when a query needs them.
- It is a small project meant to be read, run, and extended.

## Try it

```sh
cargo build --release
./target/release/shrunk demo
```

Output from this machine (Linux, release build, synthetic data):

```text
seeded 3 series x 200000 points with Counter in 76 ms; file 1.4 MiB

series                   points   blocks      stored         raw    ratio  bits/pt  codecs
cpu.load                 200000      196     1.3 MiB     3.1 MiB     2.3x    56.05  gorilla-rle 195b/1394179B, predictor 1b/7184B
requests.total           200000      196    64.2 KiB     3.1 MiB    48.7x     2.63  predictor 196b/65698B
thermostat.setpoint      200000      196    14.7 KiB     3.1 MiB   212.0x     0.60  rle 196b/15092B
total                    600000              1.4 MiB     9.2 MiB     6.5x

avg(cpu.load) over all points = 45.1299 (196 of 196 blocks read from metadata)
query cpu.load over 300 s: 300 points returned
blocks: 196 total, 2 visited, 2 decoded, 194 skipped (194 time, 0 value)
read: 2048 points from 14130 bytes of compressed payload
query whole series with value > 90 (limit 10): 10 spike points returned
blocks: 196 total, 88 visited, 10 decoded, 78 skipped (0 time, 78 value), 108 left unread after limit
read: 10240 points from 70686 bytes of compressed payload
count over the same 300 s = Some(300.0) (0 of 196 blocks answered from metadata)
```

The count query covers 300 inclusive timestamps.

One million noisy gauge points (one run; timings vary by a few percent):

```text
ingest        190.3 ms  (5.25M points/s)
save          256.3 ms  (7.8 MiB on disk)
open            1.2 ms
scan all      144.7 ms  (1000000 points returned)
avg all         0.0 ms  (977 blocks answered from metadata)
filtered        1.5 ms  (959 points, 967 blocks skipped of 977)
ratio 2.0x (65 bits/point)
```

The noisy gauge is the hard case. Counters and constants do much better because
they match the predictor, delta, and RLE codecs. Numbers vary with the data; the
benchmark commands are repeatable.

## How it works

### Blocks and codec selection

Each series is an ordered list of immutable blocks, 1024 points by default.
When a block fills, shrunk encodes it with every applicable codec and keeps the
smallest result:

| codec | id | timestamps | values | good for |
|---|---|---|---|---|
| `gorilla` | 1 | delta-of-delta, variable width | XOR of f64 bit patterns with leading/trailing zero windows | gauges, noisy floats |
| `delta-varint` | 2 | zigzag varint delta | zigzag varint delta, integral values only | counters, IDs, integer metrics |
| `rle` | 3 | start, fixed step, run length | one f64 per run | constants, flat sensors |
| `gorilla-rle` | 4 | delta-of-delta runs | Gorilla XOR | fixed-rate or regular series |
| `dolphin` | 5 | delta-of-delta runs | Gorilla XOR with a skip path for oscillating prefixes | sign-alternating waveforms |
| `predictor` | 6 | delta-of-delta runs | FCM/DFCM bit-pattern prediction plus Gorilla residual | counters, ramps, regular oscillations |
| `raw` | 0 | 16 bytes per point | 8-byte timestamp plus 8-byte f64 | incompressible fallback |

Gorilla follows the format from Facebook's Gorilla paper: the first timestamp
is 64 bits, the first delta is 64 bits, later timestamps use delta-of-delta
prefixes (`0`, `10`, `110`, `1110`, `1111`), and values reuse a previous XOR
window when the new XOR fits inside it.

`gorilla-rle` adds a run-length layer over delta-of-delta timestamps, which
collapses a fixed-step series to a single timestamp run. `dolphin` follows the
skip-encoding idea from Chen et al. (2026): when the leading-zero count is zero
and the top two XOR bits flip, it transmits the prefix once and jumps over the
redundant bits. It is only tried on blocks where at least 8% of adjacent XORs
can take that path. `predictor` follows the FCM/DFCM predictor idea used by FPC
and AFC; a 128-point sample decides whether it is worth trying on the block.
Codec IDs are persisted in the file and are never renumbered.

Every block header also stores:

- `t_min`, `t_max`: time bounds
- `v_min`, `v_max`: value bounds
- `count`: points in the block
- `sum`: total when all values are finite
- `has_nan`: disables value and min/max shortcuts when NaN is present

The block costs 58 bytes of header and checksum plus the payload. That is negligible for a 1024-point
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
shrunk seed <db> --series NAME [--pattern cpu|counter|constant|noise|saw|oscillating]
               [--points N] [--interval MS] [--start MS] [--block-size N]

shrunk ingest <db> --series NAME --file PATH

shrunk query <db> --series NAME [--from MS] [--to MS]
              [--agg count|sum|min|max|avg]
              [--gt X|--ge X|--lt X|--le X|--eq X|--ne X]
              [--limit N] [--csv] [--explain]

shrunk stats <db>
shrunk bench [--points N] [--pattern NAME] [--block-size N]
shrunk codec-bench [--points N] [--block-size N]
shrunk segment-bench [--points N] [--block-size N] [--min-seg N] [--max-seg N]
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

Writes must be strictly increasing per series. A point older than the last one
is rejected with `Error::OutOfOrder`; an equal timestamp is rejected with
`Error::Duplicate`. `write_many` validates the whole batch before applying it,
so a bad point leaves the series unchanged.

For durable incremental appends, open the database with `Db::open_rw` (or
`Db::open_rw_with_block_size`). Each `append`/`write_many` is written to a
`<db>.wal` sidecar and fsynced before it is applied in memory:

```rust
use shrunk::{Db, Point};

let mut db = Db::open_rw(std::path::Path::new("/tmp/metrics.db")).unwrap();
db.append(
    "cpu",
    &[
        Point { ts: 1_700_000_000_000, value: 41.0 },
        Point { ts: 1_700_000_001_000, value: 43.0 },
    ],
).unwrap();
// Write an atomic snapshot and truncate the WAL.
db.checkpoint().unwrap();
```

If the process dies before a checkpoint, `Db::open` replays complete WAL records
and ignores a torn or bad-CRC tail. A checkpoint writes the snapshot to a temp
file, fsyncs it, renames it, then truncates the WAL. If the crash happens
between the rename and the truncate, replay skips points already covered by the
snapshot, so recovery is idempotent.

## On-disk format

A database snapshot is one file. `save`/`checkpoint` writes a temp file,
fsyncs it, renames it, then fsyncs the directory. The current format is v3.
`FORMAT_VERSION` is the single source of truth: the magic (`SHRUNK03`), the
writer, and the reader all derive from it. v1 and v2 files are still readable
and are upgraded on the next save.

```text
magic "SHRUNK03" (8 bytes, derived from FORMAT_VERSION)
u16 format_version
u16 flags
u32 block_size
u32 series_count

per series:
  u32 name_len
  name bytes (UTF-8)
  u32 block_count

  per block:
    u8  codec           0 raw, 1 gorilla, 2 delta-varint, 3 rle,
                        4 gorilla-rle, 5 dolphin, 6 predictor
    u8  flags           bit 0 sum present, bit 1 has NaN
    u32 count
    i64 t_min
    i64 t_max
    f64 v_min
    f64 v_max
    f64 sum
    u32 payload_len
    u32 crc32(block header)
    payload
    u32 crc32(payload)
```

The separate header and payload checksums are what make lazy open safe. `open`
reads and validates every block header without touching payloads. A query reads
one payload with `pread` and checks its CRC before decoding. A corrupt header
or payload is rejected instead of being interpreted as data.

### Write-ahead log

Incremental appends use a sidecar file `<db>.wal`:

```text
"SHRUNKWL" magic (8 bytes), u16 version, u16 flags, u32 block_size
repeated:
  u32 payload_len
  payload:
    u8  record_kind (1 = points)
    u32 series_name_len
    u32 point_count
    series name bytes
    point_count * (i64 timestamp, u64 value bits)
  u32 crc32(payload)
```

Each batch is fsynced before the in-memory series is updated. Replay stops at
the first short or bad-CRC record, which is how a torn tail from a crash is
ignored. A checkpoint writes a snapshot, then truncates the WAL; replay skips
points already present in the snapshot, so a crash between those two steps is
safe. Readers that already opened a snapshot keep the old inode across a
checkpoint rename and continue to see a stable view.

## Tests

```sh
cargo test --all-targets
cargo test --doc
```

The suite covers bit-level IO, round trips for every codec (including NaN,
infinities, -0.0, and negative timestamps), randomized round trips, codec-id
stability, codec selection, time and value block skipping, aggregate results
checked against a naive fold, limits, persistence, append after reopen, lazy
payload checksums, v1/v2 migration, atomic batches, WAL recovery, torn WAL
tails, idempotent replay after a checkpoint, and snapshot-safe readers.

Reproducible compression benchmarks:

```sh
./target/release/shrunk codec-bench --points 1000000
./target/release/shrunk segment-bench --points 1000000
```

`codec-bench` reports bits/point and encode/decode cost for every codec.
`segment-bench` is the WZS-style segmentation prototype; it compares variance
segments against fixed blocks of the same average length.

## Layout

```text
src/bits.rs      bit writer/reader
src/block.rs     block construction, metadata, codec selection, lazy payloads
src/codec.rs     gorilla, delta-varint, rle, gorilla-rle, dolphin, predictor, raw
src/crc.rs       CRC-32
src/db.rs        series, writes, queries, aggregates, format versions, checkpoints
src/gen.rs       deterministic synthetic data
src/wal.rs       write-ahead log, replay, truncation
src/main.rs      CLI
tests/           integration tests and legacy fixtures
docs/            baseline and compression notes
```

## Limits

These are deliberate for a small project:

- One writer at a time. The WAL makes appends durable, but there is no
  multi-writer coordination or transaction API beyond per-batch atomicity.
- A checkpoint rewrites the whole snapshot. Incremental appends avoid that cost
  until you checkpoint, but the snapshot itself is still a full rewrite.
- `open` reads metadata only, but the block index for every series is held in
  memory. The payloads are read lazily.
- No deletes, retention windows, downsampling, or rollups.
- No multi-series joins or expression language. One series per query, one
  comparison per value filter.
- Gorilla's advantage depends on data. Noisy f64 gauges get about 2x. Integer
  counters and flat signals get 10x to 200x.
- A 54-byte block header means very small series can occupy more space than raw
  points. Use a larger block size for many tiny series.
- Aggregates ignore NaN in sum, avg, min, and max. Count counts every point.
- The predictor codec allocates two small tables per encoded/decoded block.
  That is fine at the default block size but is a future optimization target.
