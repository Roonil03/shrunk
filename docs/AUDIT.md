# Core audit

This review covers the repository as of the audited synchronization branch.
It is a code and test review, not a formal proof of the Rust implementation.
Tests exercise selected cases and cannot establish correctness for all inputs,
crashes, operating systems, or concurrent actors.

## Coverage

- `src/db.rs`: write ordering, batches, block budgets, query and aggregate
  pruning, snapshot loading and saving, compatibility and WAL replay.
- `src/wal.rs`: locking, record framing, recovery, torn-tail preservation,
  size limits and durable append/checkpoint ordering.
- `src/block.rs`, `src/codec.rs`, `src/bits.rs`: codec selection, bit and
  block boundaries, metadata and decoder contracts. Codec-specific work is
  tracked separately from the storage changes.
- `src/crc.rs`: checksums used for corruption detection; `src/gen.rs`:
  generated test/benchmark data; `src/lib.rs`: public exports and examples.
- `src/main.rs`: command parsing, file operations, output and query paths.
- `tests/` and legacy fixtures: regression and compatibility coverage;
  `Cargo.toml`, CI configuration, contribution guide, and `docs/`: build,
  supported interface, format, and documented check workflow.

## Findings addressed

The upstream review changes include lock-before-snapshot recovery and bounded
snapshot processing. This audit additionally prevents torn-tail backups from
following links or overwriting prior backups, keeps a newly created WAL after a
failed open to avoid unlinking a subsequent writer's data, rejects unordered
WAL batches before replay, bounds resynchronization memory and probe work, and
handles closed query output pipes. Regression tests accompany these changes.

## Remaining limits and follow-ups

1. Floating-point sum and average are order dependent. Metadata aggregation
   groups additions by block, so a predicate or different block size can alter
   results; exact or reproducible aggregation would require a stronger
   accumulator and an explicit format/API design.
2. Snapshot block budget projection is cached for writes; future changes to
   block construction must preserve that invariant. Benchmark high-series-count
   workloads to track performance as the implementation evolves.
3. CRC detects accidental corruption but is not authentication. Torn-tail
   classification cannot distinguish every possible corruption from a crash;
   recovered suffixes are preserved, and a bounded resync can refuse recovery
   when its work budget is exhausted. Operators should retain independent
   backups.
4. Formal verification is incremental. [Core verification](../proofs/README.md)
   records Lean models for codec algebra, byte behavior, ordered replay and
   time pruning, plus Kani checks of actual Rust encoding helpers and bounded
   storage arithmetic. It states each input domain and assumption. Full codec
   refinement, database cache maintenance, filesystem durability, concurrency
   and floating-point behavior remain unproved. Fuzzing, fault injection and
   cross-platform crash tests improve separate evidence without closing those
   formal gaps.
