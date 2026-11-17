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
and WAL truncation. Deterministic WAL faults cover each partial header length
from 1 through 15 bytes, every boundary of a single-point record, header/record
sync failures, rollback truncate/sync failures, and checkpoint truncate/sync
failures. They verify safe retry after durable rollback, mutation rejection
before memory or files change when rollback fails, and acknowledged data after
read-only and writer reopen. Isolated subprocess tests change cwd after opening
both WAL-only and snapshot-backed writers, then checkpoint, run lifecycle
maintenance, and reopen while checking that a same-named database in the new
cwd remains unchanged. A parent-symlink case also checks that successful save
does not rebind the writer before the symlink is retargeted.

Lifecycle tests compare range deletion and every downsampling aggregate with
independent reference results across block sizes, pending points, negative and
extreme timestamps, empty buckets, NaNs, and infinities. They check repeated
event-time retention, one-time rollup retry behavior, unchanged compressed
payloads, old readers, WAL-only starts, tail deletion followed by appends and
reopen, and the CLI maintenance flow. Failure injection at each snapshot stage
verifies that memory and recovery retain the same published generation.

The service is a separate Cargo crate. Run its policy unit tests as well as the
real HTTP process smoke tests:

```sh
cargo test --manifest-path service/Cargo.toml --all-targets --locked
cargo build --manifest-path service/Cargo.toml --locked
python3 service/tests/smoke.py --binary service/target/debug/shrunk-server
```

The HTTP smoke covers bounded bucket pagination, partial edge buckets and
cross-block aggregation, automatically compressed rollup storage, rejection of
oversized rollups before target creation, authenticated range/series deletion,
required deletion bounds, event-time retention, and crash/restart persistence.
A separate policy scenario checks wall-clock expiry at startup and on a timer,
future points, and no-op ticks without snapshot rewrites. The same smoke runs
against the built image in container CI, which also runs the service unit tests.
Additional fault tests require a directory durability barrier before appends,
identical delete/retention retries, and a reopened writer can proceed after a
failed post-rename sync. A reduced snapshot-size limit verifies that oversized
rollups fail before publication and leave the prior snapshot readable.
The CLI subprocess flow lives in its own integration test binary so process
creation cannot briefly inherit a WAL writer's open file description from a
parallel lifecycle test in the same test process.

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
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
cargo test --all-targets
cargo test --doc
cargo build --release
```

## Formal checks

The [core verification guide](../proofs/README.md) separates Lean
model proofs from Kani checks of actual Rust helpers, with explicit bounds,
assumptions, and remaining gaps. With Lean 4.19.0 and Kani 0.68.0 installed:

```sh
bash scripts/verify-lean.sh
cargo kani --lib
```

CI checks all standalone Lean modules with warnings treated as errors and runs
all library Kani harnesses with unwinding assertions enabled. These checks do
not replace the runtime tests or establish full-repository formal correctness.

## Linters

CI runs these linters on every pull request:

- `rustfmt` checks formatting.
- `clippy` runs the Rust lints. `Cargo.toml` denies `clippy::all` and forbids
  `unsafe` code.
- `rustdoc` treats warnings as errors, so broken doc links fail the job.
- `typos` checks spelling. Valid words that look misspelled, such as the
  `Comput.` journal abbreviation in [COMPRESSION.md](COMPRESSION.md), are listed
  in `typos.toml`.
- `actionlint` checks the workflow files in `.github/workflows/`.

Install the two tools that Rust does not ship:

```sh
cargo install typos-cli
brew install actionlint
```

Then run the linters locally:

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
typos
actionlint
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
