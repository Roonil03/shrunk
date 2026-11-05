# CLI, library, and on-disk format

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

## See also

- [Architecture.md](Architecture.md): codec selection and the query path.
- [Install.md](Install.md): build and first run.
- [Testing.md](Testing.md): tests and benchmarks.
