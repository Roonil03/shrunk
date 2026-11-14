# Core verification

There are two separate kinds of evidence here: Lean proofs of handwritten
mathematical models, and Kani checks that call selected actual Rust helpers.
Neither establishes correctness of the entire database. The coverage and
remaining gaps below are part of the verification contract.

## Lean coverage

All modules use Lean 4.19.0 and its bundled `Std` library. They are standalone
and are checked together by `scripts/verify-lean.sh`. The window cost result is
local: changing a stored window can affect later residuals and block size. The
XOR reconstruction model represents `prev_bits ^ (prev_bits ^ bits)`. The
window packing model requires removed low bits and omitted high bits to be zero;
it does not establish that Rust computes or respects these bounds.

| Module | Established property | Assumptions and boundary |
|---|---|---|
| `WindowChoice.lean` | Local compatible-window cost bounds and reuse on ties; XOR reconstruction and residual-list round trip; conditional window packing | Both window alternatives must be valid for the local cost comparison. Packing requires the explicit zero-region bounds. No Rust tags, window eligibility, or whole-stream optimality proof. |
| `BitCodec.lean` | Eight Boolean writes pack into a byte read MSB first; partial-byte alignment and zero padding; abstract prefix consumption and one exhausted read | The list reader starts after byte expansion. No theorem links this abstraction to Rust, and no arbitrary-length stateful byte-writer theorem is claimed. |
| `Storage.lean` | Ordered record partition equals filtering beyond a watermark; replay at a covered final timestamp adds nothing; time pruning and ordered-block break are safe; cache contribution replacement identity | Timestamps are mathematical integers, records strictly ordered where required, bounds honest, blocks ordered and non-overlapping. The cache identity assumes the old contribution is represented; it is not a proof of cache maintenance or filesystem replay. |

Every named theorem prints its axiom dependencies. The checked modules use
only `propext`, `Classical.choice`, and `Quot.sound`, or fewer axioms. The runner
treats warnings as errors (including admitted `sorry` proofs) and rejects
reported `sorryAx` or `Lean.ofReduceBool` dependencies. New theorem declarations
still require review and a matching `#print axioms`: the output scan is not a
general analysis of every declaration. Zigzag is checked on the Rust code below.

## Actual Rust checks

Kani 0.68.0 checks the `#[cfg(kani)]` harnesses, with loop-unwinding assertions
enabled. The normal build has no Kani runtime dependency. These are symbolic
checks of the called Rust implementation, not random tests or a translation
of its result into Lean.

| Harness | Actual target and property | Input scope |
|---|---|---|
| `codec::verification::zigzag_roundtrip` | `unzigzag(zigzag(value)) == value` | Every `i64` |
| `codec::verification::unzigzag_roundtrip` | `zigzag(unzigzag(value)) == value` | Every `u64` |
| `codec::verification::varint_roundtrip` | `put_uvarint` then `get_uvarint` returns the value and consumes every encoded byte | Every `u64`; unwind 11 |
| `codec::verification::varint_accepted_prefix_is_canonical` | Parser position stays within the input on success or error; accepted values re-encode to exactly the consumed prefix | Arbitrary ten-byte array with input length 0–10, initial position zero; unwind 11. No converse classification of every error is claimed. |
| `bits::verification::unsigned_roundtrip` | Actual writer/reader preserves low-width bits, consumes the requested width, and does not overrun | Every `u64`, widths 0–64, initially byte-aligned; unwind 66 |
| `bits::verification::signed_roundtrip` | Actual signed writer/reader returns the sign extension of the low-width bits, with correct consumption and no overrun | Every `i64`, widths 1–64, initially byte-aligned; unwind 66. Values outside the signed field range are intentionally truncated. |
| `bits::verification::reader_exhaustion` | Overrun boundary and consumed position agree with the available bytes; an exhausted next read returns false and advances the position | Arbitrary input length 0–2 bytes and 0–16 initial reads; unwind 18 |
| `db::kani_storage::projected_series_matches_point_packing` | Actual `projected_series_blocks` agrees with a point-by-point packing oracle | Storage bounds below; unwind 9 |
| `db::kani_storage::write_projection_matches_point_packing` | Actual `projected_after_write_counts`, used by `Db::projected_after_write`, agrees with an independent write/flush count simulation and preserves other series' contribution | Storage bounds below; unwind 9 |
| `db::kani_storage::flush_delta_matches_point_packing` | Actual `Series::flush_block_delta` agrees with point packing | Storage bounds below; unwind 9. Synthetic blocks supply counts only; their payloads are not decoded. |

Storage checks cover block sizes 1–4, existing block counts 0–4, last count zero
when there are no blocks and otherwise 1–block size, pending length below block
size, and new point counts 0–4. The write-projection check also covers a symbolic
other-series contribution up to `MAX_TOTAL_BLOCKS`, with the initial total
within that limit. The projected output may exceed the budget: enforcement of
the resulting limit is separate. The cache is assumed initially consistent.

The storage oracle packs points individually; it does not call the projection
helpers. Extracting the existing write-projection arithmetic into a private
helper makes that production calculation directly checkable without modeling a
`BTreeMap`. Database map wiring, payload encoding, and actual `Series::flush`
execution are covered by ordinary tests, not these formal checks.

Kani checks depend on its supported compiler, CBMC, and standard-library and
environment models; they are not Lean kernel proofs. Bounds, synthetic states,
and the verifier's allocation/environment modeling remain part of the scope.

## Running the checks

With Lean 4.19.0 on the path:

```sh
bash scripts/verify-lean.sh
```

Or set `LEAN` to an absolute Lean executable path. The runner checks every
`proofs/*.lean` module. CI retains the pinned Lean archive checksum.

For the actual Rust checks on a Kani-supported host:

```sh
cargo install --locked kani-verifier --version 0.68.0
cargo kani setup
cargo kani --lib
```

Kani installs its supported compiler separately from the ordinary Rust
toolchain. CI runs the same pinned version. Do not disable unwinding assertions
or omit failing harnesses to obtain a successful result.

## Remaining verification work

- Link complete Rust codec framing, window state, timestamp arithmetic and
  byte streams to an end-to-end specification, including non-aligned fields.
- Extend storage arithmetic beyond the stated small-state bounds and prove
  cache initialization and updates across real database operations.
- Prove WAL framing, snapshot generations, lock behavior and crash/durability
  transitions against an explicit filesystem/concurrency model.
- Establish value-predicate and floating-point aggregation semantics. Integer
  timestamp pruning proofs do not verify IEEE-754 behavior.

The runtime tests, legacy fixtures and corruption checks remain necessary;
their success is separate empirical evidence and does not close these gaps.
