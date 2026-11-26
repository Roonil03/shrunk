# Compression and durability notes

## Canonical Gorilla window path optimization (2026-11-15)

Gorilla-RLE now chooses XOR value windows with a dynamic program for blocks of
at most 2048 points. The on-disk codec ID and bit grammar are unchanged, so
existing readers can decode the new payloads. Longer blocks use the previous
greedy encoder. Timestamp encoding and every other codec are unchanged.

For each nonzero XOR, a **fresh** action installs its canonical tight window:
`lz = min(leading_zeros, 31)`, `tz = trailing_zeros`, width `64-lz-tz`.
It costs `13 + width` bits (two tags, eleven metadata bits, payload). A
**reuse** action is legal when the old window contains the XOR and costs
`2 + old_width` bits. A zero XOR costs one bit and leaves the state unchanged.
The dynamic program minimizes total bit cost over these actions, including
future effects of a window choice. Equal canonical windows are merged; states
that cannot reuse a residual are discarded. Backtracking stores only the
winning fresh predecessor per residual because reuse is a self-loop. Time is
`O(nw)` and extra memory `O(n+w)`, where `w ≤ 1552` is the number of possible
canonical windows. The 2048-point cap bounds latency for configurable blocks.

The old greedy encoding is one admissible path. Thus, on optimized blocks,
the new value bit count is no greater than the old one; identical timestamp
bytes and final byte padding preserve the payload-size inequality. On longer
blocks the bytes are identical. The Predictor eligibility gate still compares
its 128-point prefix against greedy Gorilla-RLE. This preserves which full
Predictor candidates `Block::build` tries; the Gorilla-RLE candidate itself
uses the optimized path. With the same candidate set and no larger Gorilla-RLE
payload, selected block payload size cannot increase because of this change.
The proof in
`proofs/OptimalWindowPath.lean` establishes that the canonical finite-horizon
minimum is both attainable and no greater than any valid path. Its concrete
step model charges the Gorilla fresh/reuse bit costs and checks compatibility,
under valid tight-window bounds. Existing window reconstruction
proofs remain in `proofs/WindowChoice.lean`. A machine-checked refinement from
the Rust state table and byte writer to that abstract recurrence is still
missing. Exhaustive tests compare the Rust choice with brute force on 1296
four-residual sequences; round-trip tests cover IEEE-754 bit patterns.

This is an exact optimizer for the **canonical-window subgrammar**. Gorilla's
wire format can represent wider fresh windows, so the result is not a global
optimum over every representable payload or over other codec families. The
dynamic-programming technique itself is established; no claim of a new
general-purpose compression algorithm follows from this change.

Measured with `codec-bench --points 100000`, block size 1024, release build,
using a fresh `origin/main` archive as baseline on the same machine. These
single-run timings vary with system load; sizes are deterministic.

| pattern | greedy Gorilla-RLE bits/pt | optimized bits/pt | greedy encode ns/pt | optimized encode ns/pt |
|---|---:|---:|---:|---:|
| cpu | 53.91 | 53.09 | 86.3 | 131.2 |
| counter | 11.25 | 8.24 | 16.6 | 51.9 |
| noise | 58.50 | 58.14 | 61.4 | 149.2 |
| saw | 54.74 | 54.25 | 62.7 | 112.4 |

On `bench --points 100000 --pattern cpu`, the selected whole file fell from
663.7 to 653.7 KiB (54.37 to 53.55 bits/point). Encode throughput is the
main cost of the optimization; decode needs no new logic. On counter and saw,
the predictor still wins block selection despite the Gorilla-RLE improvement.

The sections below preserve measurements from earlier implementation stages.
The commands document how those runs were made; current results may differ.

Environment for the whole-file and per-codec measurements below:

- Linux Mint 22.3, x86_64, 8 logical CPUs, 7.6 GiB RAM
- rustc/cargo 1.99.1, `--release` (`opt-level=3`, LTO, `panic=abort`)
- `--points 1000000`, block size 1024, one series
- Timings are from one run.

Commands:

```sh
cargo build --release
./target/release/shrunk bench --points 1000000 --pattern <pattern>
./target/release/shrunk codec-bench --points 1000000
./target/release/shrunk segment-bench --points 1000000
```

## Whole-file results

`bench` includes ingest, an atomic snapshot, lazy open, a full scan, a metadata
aggregate, and a filtered query. "Before" is `docs/BASELINE.md`.

| pattern | before bits/pt | after bits/pt | before file | after file | after ratio | after open |
|---|---:|---:|---:|---:|---:|---:|
| cpu         | 57.00 | 56.00 | 6.8 MiB   | 6.7 MiB   |  2.3x | 1.7 ms |
| counter     | 12.72 |  2.63 | 1.5 MiB   | 320.5 KiB | 48.8x | 1.1 ms |
| constant    |  0.57 |  0.60 | 69.7 KiB  | 73.5 KiB  | 212.7x | 1.5 ms |
| noise       | 66.07 | 65.06 | 7.9 MiB   | 7.8 MiB   |  2.0x | 1.7 ms |
| saw         | 56.23 | 31.32 | 6.7 MiB   | 3.7 MiB   |  4.1x | 1.8 ms |
| oscillating | n/a   | 59.95 | n/a       | 7.1 MiB   |  2.1x | 1.2 ms |

The large wins are the predictor codec on counters/ramps and the GR timestamp
runs. CPU and noise are dominated by noisy mantissa bits, which no lossless
codec can remove. The constant series is slightly larger than the baseline
because the v3 block header carries a separate header CRC and the file header is
larger; RLE still wins the payload by a wide margin.

Peak RSS for the whole `bench` process was 42-50 MiB. `open` no longer reads
payloads, so open time is 1-2 ms even at one million points (nonconstant baseline
8-54 ms).

## Per-codec results (payload only)

From `codec-bench --points 1000000`. `*` marks the smallest payload for that
pattern.

| pattern | gorilla | gorilla-rle | dolphin | predictor | rle | best |
|---|---:|---:|---:|---:|---:|---|
| cpu         | 56.58 | 55.55* | 55.56 | 56.54 | 104.04 | gorilla-rle |
| counter     | 12.30 | 11.26  | 11.27 |  2.17* | 104.04 | predictor |
| constant    |  2.19 |  1.16  |  1.16 |  2.16  |   0.15* | rle |
| noise       | 65.65 | 64.62* | 64.63 | 65.57  | 104.04 | gorilla-rle |
| saw         | 55.81 | 54.78  | 54.88 | 30.86* | 104.04 | predictor |
| oscillating | 67.14 | 66.11  | 65.06 | 61.03* | 104.04 | predictor |

Encode/decode cost at the same settings (ns per point, payload only):

| pattern | gorilla-rle | dolphin | predictor |
|---|---:|---:|---:|
| cpu         | 66 / 75 | 65 / 74 | 72 / 82 |
| counter     | 22 / 22 | 21 / 20 | 16 / 10 |
| constant    |  5 /  7 |  4 /  3 | 20 / 10 |
| noise       | 71 / 83 | 73 / 88 | 80 / 94 |
| saw         | 66 / 75 | 65 / 72 | 49 / 50 |
| oscillating | 75 / 85 | 77 / 94 | 82 / 85 |

The predictor is both smaller and faster on the patterns where it wins. Dolphin
beats Gorilla and Gorilla-RLE on this oscillating workload, but predictor has
the smallest payload. Dolphin costs more decode time there; it is only tried
when at least 8% of adjacent XORs can take the skip path.

## Implemented

### Validation and format

- Block metadata is validated on read: non-zero count, count/size limits,
  `t_min <= t_max`, non-NaN value bounds, `v_min <= v_max` when `has_nan` is
  false, finite stored sums, codec id known.
- v3 stores a CRC over the block header and a separate CRC over the payload.
  `open` validates headers; queries validate payloads lazily.
- v1 and v2 decoders are retained. Saving upgrades a legacy database to the
  current format.
- `FORMAT_VERSION` is the single source of truth. The magic is derived as
  `SHRUNK` plus two decimal digits, so a version bump is one constant plus an
  optional legacy decoder. `FORMAT_MAGIC` is exported for tests.
- Size limits reject absurd series counts, names, block counts, block sizes, and
  payload lengths before allocating.

### Write semantics

- Timestamps must strictly increase per series.
- A duplicate timestamp returns `Error::Duplicate`; an older timestamp returns
  `Error::OutOfOrder`.
- `write_many` validates the whole batch first, then applies it. A failed batch
  leaves memory and the WAL unchanged.
- The CLI `ingest` path uses one atomic batch, so a bad line cannot leave a
  partially ingested file behind.

### Durability

- `<db>.wal` is an append-only sidecar with a versioned header and CRC'd point
  records.
- `Db::open_rw` / `open_rw_with_block_size` take the exclusive WAL lock before
  reading the snapshot, then bind the database for appends. `append`, `write`,
  and `write_many` fsync the WAL before applying to memory. There is no group
  commit: each batch is one `fsync`.
- An invalid WAL record is treated as a torn crash tail only when a resync of
  the suffix finds no later CRC-valid record. `Db::open` ignores that tail;
  `open_rw` copies it to `<db>.wal.torn` (or a numbered `.torn.N` backup if that name exists) and then truncates it. Damage with a
  later valid record, including a corrupted length that no longer points at
  the next record, is still a hard error and is preserved. A length or CRC
  failure of the last record is indistinguishable from a crash mid-append and
  may drop that last record.
- `Db::open` re-checks the snapshot and WAL after the read. It retries only
  if the snapshot changed, or the WAL was replaced or shrunk. Growth of an
  existing WAL is a consistent prefix and does not fail the open.
- A checkpoint writes a snapshot atomically (temp file, fsync, rename, directory
  fsync) and then truncates the WAL.
- Replay is idempotent: points already covered by the snapshot are skipped, so a
  crash between the rename and the truncate is safe.
- A reader that opened a snapshot keeps the old inode across a checkpoint rename
  and sees a stable view.

### Codecs

Codec ids are persisted and must never be renumbered.

| id | name | notes |
|---:|---|---|
| 0 | `raw` | 16 bytes/point fallback |
| 1 | `gorilla` | original delta-of-delta timestamps + XOR values |
| 2 | `delta-varint` | integral values only |
| 3 | `rle` | fixed-step runs |
| 4 | `gorilla-rle` | GR timestamp runs + Gorilla values |
| 5 | `dolphin` | Dolphin-inspired prefix skip, gated at 8% skip rate |
| 6 | `predictor` | FCM/DFCM prediction + Gorilla residual, gated by a 128-point sample |

## Research verdicts

### GR / fixed-step timestamp RLE — implemented

The GR idea (delta-of-delta runs) collapses a fixed-step timestamp column to a
single run. It is a 1-2% payload win over Gorilla on every non-constant pattern
and costs about 5% more decode time. Kept as codec id 4.

### Dolphin — implemented, gated

Dolphin (Chen et al., 2026) targets oscillating floating-point series. When the
leading-zero count of the XOR is zero and the top two bits flip, bits 2-8 are
often zero. Dolphin transmits the two-bit prefix once and jumps over the
redundant nine-bit prefix.

The paper is open access and specifies the skip mechanism; the exact ACTF branch
table was not needed for this prototype. Our version adds a 2-bit value tag
(reuse / normal window / skip) on top of the Gorilla XOR stream and shares the GR
timestamp stream. It wins 1.6% on the synthetic oscillating pattern and is not
selected on cpu/counter/noise/saw. It is gated so blocks that cannot benefit do
not pay the extra bit per new window.

### WZS — prototyped, not integrated

WZS (Wang et al., 2026) combines adaptive segmentation, a genetic-algorithm
parameter optimizer, and streaming algorithm selection. The full method is
paywalled. We prototyped the segmentation idea: cut where the local
leading-zero variance changes, then encode each segment with the best codec.

`segment-bench` compares variance segments against fixed blocks of the same
average length. At thresholds 16, 4, and 1 the change was between +0.08% and
-1.06%; the only repeatable win is about 1% on oscillating data, while the
lowest threshold also makes several patterns much worse in absolute terms
(counter 2.16 -> 6.94 bits/point) because segments shrink to ~127 points.
Conclusion: no consistent measurable win across workloads, so segmentation is
not part of the default write path. The benchmark command remains for future
work.

### AFC — predictor prototyped and integrated

AFC (Chen et al., 2024) uses four adaptive strategies; the paper is paywalled.
Its predictor lineage is public through FPC: FCM maps a context to the last value
seen in it, DFCM maps a context to the last bit-pattern delta, and the predictor
with the longer zero prefix wins. We implemented that idea independently as codec
id 6.

The result is the largest win in this work: 2.63 bits/point on a counter versus
11.72 before (48.8x versus 10.9x). A 128-point sample encode gates the codec, so
unpredictable blocks pay about 12% extra ingest time instead of a full predictor
pass.

The comparator for the earlier 11.72 bits/point (10.9x) figure is not identified
here; the named [pre-hardening baseline](BASELINE.md) lists 12.72 bits/point
(10.1x).

### ARIMA/RL quantization and LSCodec

Not implemented or integrated into the write path; the available codecs are
lossless.

## Trade-offs and remaining gaps

- Ingest is slower on non-predictable data because the predictor gate encodes a
  128-point sample. CPU ingest went from about 175 ms to 191 ms at 1M points.
- The predictor reuses two 4096-entry tables on each thread that runs the codec.
  Every block starts with cleared tables.
- A checkpoint still rewrites the whole snapshot. The WAL avoids that cost
  between checkpoints, but there is no incremental compaction yet.
- The WAL has one writer and no group commit. Each batch is fsynced
  individually.
- The Dolphin prototype is not a full ACTF implementation; it captures the skip
  mechanism but not every branch of the paper.
- WZS parameter optimization (genetic algorithm) was not reproduced.
- The AFC prototype is FCM/DFCM-based, not a verified reproduction of all four
  AFC strategies.
- The measurements in this document use synthetic workloads. See
  [the real sensor benchmark](Testing.md#real-sensor-benchmark) for the checked-in
  Beijing PM2.5 trace; these measurements do not establish general production
  performance.
- A `cargo fuzz` target is not checked in; randomized deterministic round-trip
  tests cover codec correctness, and integration tests cover corruption and
  crash recovery.

## Test commands

```sh
cargo fmt --check
cargo test --all-targets
cargo test --doc
cargo build --release
./target/release/shrunk codec-bench --points 1000000
./target/release/shrunk segment-bench --points 1000000
```

## Citations

- Pelkonen et al. Gorilla: a fast, scalable, in-memory time series database.
  PVLDB 8(12), 2015.
- Liakos, Papakonstantinopoulou, Kotidis. Chimp: efficient lossless floating
  point compression for time series databases. PVLDB 15(11), 2022.
- Burtscher, Ratanaworabhan. FPC: a high-speed compressor for double-precision
  floating-point data. IEEE Trans. Computers 58(1), 2008.
- Chen, Liu, Meng, Lu. AFC: an adaptive lossless floating-point compression
  algorithm in time series database. Information Sciences 654, 2024.
  https://doi.org/10.1016/j.ins.2023.119847
- Chen, Yan, Jung, Zhao. Dolphin: an adaptive lossless compression algorithm for
  oscillating floating-point time series. J. King Saud Univ. Comput. Inf. Sci.
  38(5), 2026. https://doi.org/10.1007/s44443-026-00553-5
- Wang, Chen, Zhao, Zhang, Lin. Waved zero streaming: a time series compression
  framework with adaptive segmentation optimization. Information Processing &
  Management 63(6), 2026. https://doi.org/10.1016/j.ipm.2026.104697
- Zhou, Li, Xu, Li, Zhang. GR: a fixed-frequency time series data timestamp
  compression algorithm based on RLE. ICCAIS 2025, 2026.
  https://doi.org/10.1117/12.3111295

## Historical cost-based Gorilla window updates

This records the earlier greedy window update. The canonical path optimization
described above supersedes it for Gorilla-RLE blocks of at most 2048 points.

At this stage, the Gorilla XOR encoder compares the cost of reusing a compatible
window with the cost of declaring a new one: the old window width versus 11
metadata bits plus the new width. Both alternatives use the same two tag bits. Ties
reuse the old window. The existing wire grammar and decoders are unchanged.
The choice minimizes the current residual's bits, not the entire stream: a
narrower window can make a later residual more expensive. See
[`proofs/README.md`](../proofs/README.md) for the Lean model and its limits.

A single synthetic run on Linux x86_64 (8 logical CPUs), rustc 1.99.1,
release mode, block size 1024, 100,000 points, using the checked-in CLI:

```sh
cargo build --release
./target/release/shrunk codec-bench --points 100000
./target/release/shrunk bench --points 100000 --pattern cpu
./target/release/shrunk bench --points 100000 --pattern oscillating
```

The baseline binary was built before the encoder change (commit `aeb8d05`;
upstream changes through `1e7c834` do not alter codec logic). Payload sizes
from `codec-bench` were:

| pattern / codec | before bits/point | after bits/point |
|---|---:|---:|
| CPU / Gorilla-RLE | 55.63 | 53.91 |
| noise / Gorilla-RLE | 64.55 | 58.50 |
| counter / predictor | 2.17 | 2.17 |
| saw / predictor | 30.96 | 28.74 |
| oscillating / predictor | 61.04 | 41.05 |

Whole-file CPU changed from 56.09 to 54.37 bits/point; oscillating changed
from 59.97 to 41.51. These are synthetic results, not a universal size or
throughput guarantee; timings were affected by shared system load and are not
compared here.
