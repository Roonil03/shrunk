# Testing and benchmarks

## Test suite

```sh
cargo test --all-targets
cargo test --doc
```

The suite covers bit-level IO, round trips for every codec (including NaN,
infinities, -0.0, and negative timestamps), randomized round trips, codec-id
stability, codec selection, time and value block skipping, aggregate results
checked against a naive fold, limits, persistence, append after reopen, lazy
payload checksums, v1/v2 migration, atomic batches, WAL recovery, short length
prefix repair, rejection of ambiguous truncation and corrupt lengths,
idempotent replay after a checkpoint, and snapshot-safe readers. Bounded
synthetic cases also cover float-bit preservation, varint boundaries, sparse
WALs, WAL symlinks and permissions, writer locking, stale destination WALs,
and injected snapshot failures before write, sync, rename, directory sync,
and WAL truncation.

Run one test while iterating:

```sh
cargo test codec::tests::randomized_roundtrips_for_every_codec
cargo test --test integration wal
```

Legacy format fixtures live in `tests/fixtures/`:

- `v1.shrunk`: a v1 database used by the migration test.
- `v2.shrunk`: a v2 database used by the migration test.

Before opening a pull request, run the full check suite:

```sh
cargo fmt --check
cargo test --all-targets
cargo test --doc
cargo build --release
```

## Reproducible compression benchmarks

Build in release mode first:

```sh
cargo build --release
```

Then run the benchmark commands:

```sh
./target/release/shrunk codec-bench --points 1000000
./target/release/shrunk segment-bench --points 1000000
```

`codec-bench` reports bits/point and encode/decode cost for every codec.
`segment-bench` is the WZS-style segmentation prototype; it compares variance
segments against fixed blocks of the same average length.

For end-to-end numbers, including ingest, save, lazy open, a full scan, and
queries:

```sh
./target/release/shrunk bench --points 1000000 --pattern cpu
```

Patterns are `cpu`, `counter`, `constant`, `noise`, `saw`, and `oscillating`.
Timings vary by a few percent between runs; record the command and the hardware
when you publish results. Measured results and the research verdicts behind
each codec are in [COMPRESSION.md](COMPRESSION.md).

## See also

- [Architecture.md](Architecture.md): how the codecs and query path work.
- [Info.md](Info.md): CLI and library usage.
- [CONTRIBUTING.md](../CONTRIBUTING.md): the full pull request checklist.
