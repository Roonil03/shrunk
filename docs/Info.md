# CLI, library, and on-disk format

## CLI

```text
shrunk seed <db> --series NAME [--pattern cpu|counter|constant|noise|saw|oscillating]
               [--points N] [--interval MS] [--start MS] [--block-size N]

shrunk ingest <db> --series NAME --file PATH

shrunk query <db> --series NAME [--from MS] [--to MS]
              [--agg count|sum|min|max|avg]
              [--gt X|--ge X|--lt X|--le X|--eq X|--ne X]
              [--every MS] [--limit N] [--csv] [--explain]

shrunk delete <db> --series NAME [--from MS] [--to MS] [--drop-series]
shrunk retain <db> --since MS
shrunk retain <db> --now MS --window MS
shrunk rollup <db> --series NAME --target NAME --every MS
              --agg count|sum|min|max|avg [--from MS] [--to MS]

shrunk stats <db>
shrunk bench [--points N] [--pattern NAME] [--block-size N]
shrunk codec-bench [--points N] [--block-size N]
shrunk segment-bench [--points N] [--block-size N] [--min-seg N] [--max-seg N]
shrunk demo [--points N]
```

Timestamps are milliseconds and time bounds are inclusive. `ingest` accepts
strict two-column `timestamp,value` rows, with an optional exact header on the
first non-comment line. Blank lines and lines beginning with `#` are ignored;
invalid rows report their physical line number. Input is limited to 64 MiB and
one million points and is parsed before any database write.

Example:

```sh
shrunk seed /tmp/cpu.db --series cpu --points 100000 --pattern cpu
shrunk stats /tmp/cpu.db
shrunk query /tmp/cpu.db --series cpu --from 1700000000000 --to 1700000600000 --agg avg --explain
shrunk query /tmp/cpu.db --series cpu --from 1700000000000 --to 1700000600000 --gt 60 --limit 5
shrunk query /tmp/cpu.db --series cpu --every 60000 --agg avg --csv --explain
shrunk rollup /tmp/cpu.db --series cpu --target cpu.minute --every 60000 --agg avg
shrunk delete /tmp/cpu.db --series cpu --from 1700000000000 --to 1700000059999
shrunk retain /tmp/cpu.db --now 1700086400000 --window 86400000
```

`delete` removes points in the inclusive range, retaining the series name even
when empty. Omitted bounds cover all timestamps. `--drop-series` removes the
name as well and cannot be combined with time bounds. Later appends must be
newer than the last surviving point; deleting the tail permits those timestamps
to be written again.

`retain` applies to every series. `--since MS` keeps timestamps at or after the
cutoff; `--now MS --window MS` computes the cutoff as `now - window`, saturating
at the minimum i64 timestamp. The window must be positive. This is explicit
maintenance based on caller-supplied event time, including future-dated points;
it does not consult the wall clock or enforce a policy on later writes. Repeating
the same retention is a no-op. Rollup targets are ordinary series and retention
applies to their bucket timestamps too.

`query --every MS --agg ...` produces one aggregate per nonempty bucket and
supports `--csv` and `--explain`. It cannot be combined with a value filter or
`--limit`. Buckets are fixed width and aligned to timestamp zero: width 10 groups
timestamps -10 through -1 under -10, and 0 through 9 under 0. Only source points
inside the inclusive query range participate, even for partial edge buckets.
Output timestamps mark bucket starts and can precede `--from`. A mathematical
bucket start below i64::MIN is represented as i64::MIN. Empty buckets are omitted;
average, min, and max also omit buckets containing only NaNs. Count includes NaNs,
sum ignores them, and infinities follow the ordinary aggregate behavior.

`rollup` materializes those points into a new, distinct target series. It leaves
the source unchanged and does not refresh automatically. An empty result still
creates an empty target. Existing targets are rejected, making a repeated request
an error rather than appending duplicate or overlapping buckets. Inspect an
existing target after an uncertain result, or explicitly delete it before
rebuilding. A rollup cannot be safely averaged again without retaining each
bucket's source count; the stored points are aggregate values, not weighted
aggregate states.

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
so a bad point leaves the series unchanged. A batch is limited to one million
points; generated CLI workloads are written in chunks of that size. A query
returning more than 16 million points errors rather than growing without bound.

`flush_all` returns `Result<()>`; callers should handle failures (for example,
`db.flush_all()?`). `Error` is non-exhaustive, so external exhaustive matches
need a wildcard arm. These are source-level API changes from earlier versions.
Sum and average use floating-point arithmetic. Block metadata groups additions,
so changing block boundaries or adding a predicate can change rounding; these
aggregates do not promise identical bits across query plans.

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

If the process dies before a checkpoint, `Db::open` replays complete WAL records.
An invalid record is treated as a torn crash tail only when a resync of the
suffix finds no later CRC-valid record: `open` ignores that tail and leaves
the file alone, and `open_rw` copies it to `<db>.wal.torn` (or a numbered `.torn.N` backup if that name exists) before truncating
and appending. A length or CRC failure of the last record is indistinguishable
from a crash mid-append and may drop that last record. Damage with a later
valid record, including a corrupted length field, is still a hard error and is
preserved for inspection. `open` retries if the snapshot changes, or if the
WAL is replaced or shrinks, so a concurrent checkpoint cannot produce a mixed
view. Growth of an existing WAL is a consistent prefix and does not fail the
open. A checkpoint writes the snapshot to a new private temp file, fsyncs it,
renames it, syncs the parent directory, then truncates the WAL. If a crash
happens between rename and truncation, replay skips points already covered by
the snapshot, so recovery is idempotent.

`Db::new` followed by `save` creates or replaces a snapshot only when its
destination has no WAL. An unbound handle can also export its current contents
to another WAL-free path; the original path is left intact. It refuses a
destination WAL, including WAL-only data;
use `Db::open_rw` and `checkpoint` to persist recovered appends. A WAL-bound
handle can save only to its bound destination. The CLI `seed` requires an unused
snapshot path and also refuses an existing WAL. `open_rw` holds an exclusive WAL
file lock from recovery through append and checkpoint, so a second writer to
the same WAL fails. Snapshot symlinks and hard-link aliases are rejected for
writes. This is a single-writer API; separate names referring to the same
storage through other alias mechanisms are outside its coordination contract.

### Lifecycle API and durability

```rust
use shrunk::{Agg, Db};

let mut db = Db::open_rw(std::path::Path::new("/tmp/metrics.db")).unwrap();
// The source series must already exist.
let minute = db.downsample("cpu", 0, i64::MAX, 60_000, Agg::Avg).unwrap();
db.rollup("cpu", "cpu.minute", 0, i64::MAX, 60_000, Agg::Avg).unwrap();
let removed = db.delete_range("cpu", 0, 59_999).unwrap();
println!("{} points removed", removed.points_deleted);
db.retain_window(1_700_086_400_000, 86_400_000).unwrap();
db.retain_since(1_700_000_000_000).unwrap();
// Drop both the data and the series name.
db.delete_series("cpu.minute").unwrap();
```

`delete_range`, `retain_since`, and `retain_window` return `DeleteResult` with
`points_deleted`, `blocks_dropped`, and `blocks_rewritten` counters. Full blocks
inside a deleted range are removed without decoding, and unaffected blocks keep
their codec and compressed payload. Only boundary blocks are decoded and rebuilt.
`downsample` returns `QueryResult`, including query statistics, and uses header
aggregates for full blocks contained in one bucket. Its output is limited to
16 million buckets. Floating-point rounding has the same query-plan dependence
as ordinary aggregates. All lifecycle operations use the existing codecs.

`downsample_with_limit(series, start, end, interval_ms, agg, limit)` returns
only a prefix of completed buckets and stops scanning once that prefix is
available. It bounds allocation during execution; callers can request one extra
bucket to detect truncation. Continue from the next epoch-aligned bucket boundary
after the last returned timestamp, rather than from that timestamp plus one.
Zero returns no buckets, and limits above 16 million are rejected.
`rollup_with_limit(source, target, start..=end, interval_ms, agg, max_buckets)`
instead requires the entire result to fit its positive budget, returning
`Error::Limit` without installing a partial target when it exceeds the budget.
`oldest_timestamp()` returns the earliest surviving timestamp or `None`, reading
only series metadata and pending points. Retention skips series whose earliest
point is already within the window, avoiding payload copies for no-op checks.

For a handle from `open_rw`, destructive maintenance and rollups are durable
when they return successfully. Changes are prepared and validated first. The
writer then checkpoints the old state and durably empties the WAL, followed by
an atomic replacement snapshot with the new state. Both snapshots are written
while holding the existing writer lock. Emptying the old WAL before publishing
deletions prevents removed tail points from being replayed after a crash.
This costs up to two full snapshot rewrites and introduces no new file or WAL
format. Readers can observe the state before or after the atomic replacement;
an existing reader retains its old snapshot.

As with checkpoint, an I/O error after rename can mean the new state is already
visible. The writer retains that visible state in memory; an error before rename
restores the old state. After a failed directory sync, the same writer retries
that sync before acknowledging any new WAL append or successful maintenance
retry. A reopened writer syncs the snapshot directory under its writer lock
before replay or appends. Reopen and inspect after an uncertain durability result.
Handles from `new` or `open` apply lifecycle changes in memory until explicitly
saved. Retention and range deletion retain empty series names; `delete_series`
removes them and permits fresh writes at any timestamp.

## On-disk format

A database snapshot is one file. `save`/`checkpoint` streams its blocks to an
exclusive same-directory temp file, fsyncs it, renames it, then fsyncs the
directory. The current format is v3.
The writer rejects a snapshot larger than the reader's 64 GiB limit before
renaming it into place.
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

The separate header and payload checksums make lazy open safe. `open`
reads and validates every block header without touching payloads. A query reads
one payload with `pread` and checks its CRC before decoding. A corrupt header
or payload is rejected instead of being interpreted as data. These checksums
detect accidental corruption; they do not authenticate files. Legacy v1
headers have no checksum, so v1 query metadata is re-derived from decoded
payloads before queries can use it. Legacy v1/v2 snapshots remain readable;
v1 whole-file loading has a 256 MiB read budget. Current snapshots have a
one-million-block index budget and a 64 GiB file-size ceiling; each block's
payload is limited to 64 MiB.

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

Each batch is fsynced before the in-memory series is updated. There is no group
commit. WAL version 1 framing and codec IDs are unchanged. Replay ignores a
torn terminal record after a suffix resync finds no later valid record, and
`open_rw` copies that tail to `<db>.wal.torn` (or a numbered `.torn.N` backup if that name exists) before truncating. Damage with
a later valid record errors out and is preserved. WAL creation uses mode
`0600` on Unix; reads and writes reject non-regular WAL files and
final-component symlinks. `O_NOFOLLOW` uses the correct architecture-specific
value, and the opened descriptor is checked against the path on Unix.
Recovery and appends have a 1 GiB WAL size ceiling. A checkpoint writes a
snapshot, then truncates the WAL; replay skips points already present in the
snapshot, so a crash between those two steps is safe. Readers that already
opened a snapshot keep the old inode across a checkpoint rename and continue
to see a stable view.

## See also

- [Architecture.md](Architecture.md): codec selection and the query path.
- [Install.md](Install.md): build and first run.
- [Testing.md](Testing.md): tests and benchmarks.
