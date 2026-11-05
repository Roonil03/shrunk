# Compression and durability notes

This file records what was implemented, what was measured, and what was
rejected. All numbers come from the checked-in benchmark commands, so they can
be reproduced on another machine.

Environment for every measurement below:

- Linux Mint 22.3, x86_64, 8 logical CPUs, 7.6 GiB RAM
- rustc/cargo 1.99.1, `--release` (`opt-level=3`, LTO, `panic=abort`)
- `--points 1000000`, block size 1024, one series
- Timings are one run and vary by a few percent between runs.

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
payloads, so open time is 1-2 ms even at one million points (baseline 8-54 ms).

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
wins on oscillating data but costs more decode time there; it is only tried when
at least 8% of adjacent XORs can take the skip path.

## Implemented

### Validation and format

- Block metadata is validated on read: non-zero count, count/size limits,
  `t_min <= t_max`, `v_min <= v_max`, finite bounds, codec id known.
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
- `Db::open_rw` / `open_rw_with_block_size` bind a database for appends.
  `append`, `write`, and `write_many` fsync the WAL before applying to memory.
- Replay stops at the first short or bad-CRC record, so a torn crash tail is
  ignored.
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
single run. In the payload-only benchmark, gorilla-rle is 1.5-1.9% smaller
than Gorilla on cpu, noise, saw, and oscillating patterns, and 8.5% smaller on
counter. It costs about 5% more decode time. Kept as codec id 4.

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

The result is the largest win in this work: the whole-file counter size is 2.63
bits/point versus the 12.72 bits/point baseline. Its raw-data compression ratio
is 48.8x, compared with 10.1x before. A 128-point sample encode gates the codec, so
unpredictable blocks pay about 12% extra ingest time instead of a full predictor
pass.

### ARIMA/RL quantization and LSCodec

Not enabled. The existing lossy experiments remain optional and are not part of
the default write path. No lossy codec is selected unless explicitly requested.

## Trade-offs and remaining gaps

- Ingest is slower on non-predictable data because the predictor gate encodes a
  128-point sample. CPU ingest went from about 175 ms to 191 ms at 1M points.
- The predictor allocates two 4096-entry tables per encoded/decoded block. This
  is a future optimization; a reusable scratch buffer would remove it.
- A checkpoint still rewrites the whole snapshot. The WAL avoids that cost
  between checkpoints, but there is no incremental compaction yet.
- The WAL has one writer and no group commit. Each batch is fsynced
  individually.
- No concurrent writer coordination, deletes, retention, or downsampling.
- The Dolphin prototype is not a full ACTF implementation; it captures the skip
  mechanism but not every branch of the paper.
- WZS parameter optimization (genetic algorithm) was not reproduced.
- The AFC prototype is FCM/DFCM-based, not a verified reproduction of all four
  AFC strategies.
- Benchmarks are synthetic. Real sensor traces should be added before claiming
  production numbers.
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
