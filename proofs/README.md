# Window cost model

`WindowChoice.lean` targets Lean 4.19.0, with only the bundled `Std` library.
Run `lean proofs/WindowChoice.lean` from the repository root.

The kernel-checked model establishes that, when both representations are
valid, the chosen local bit cost is no greater than either alternative, and
that ties choose reuse. It also proves the 64-bit XOR reconstruction identity
and lossless window packing under explicit bounds for the zero regions.
This is a local result, not a global optimality claim: changing the stored
window affects later residuals, so an entire block can in principle grow.

The model is handwritten; no theorem connects it mechanically to the Rust
encoder, decoder, byte packing, floating-point bit conversions, or block
selection. The Rust tests and benchmark supply separate empirical evidence,
not a proof of refinement or full Rust verification.

`xor64_reconstruct` models `prev_bits ^ (prev_bits ^ bits)` used by
`encode_xor` and `decode_xor`. `window_roundtrip` models shifting away trailing
zeros, writing the low `width` bits, reading them and shifting back. Its `low`
and `high` hypotheses state that removed low bits and omitted high bits are
zero. The Lean proof does not establish that Rust computes or respects those
bounds; it also does not cover malformed input handling, window state, or
padding. `#print axioms` reports only Lean's standard `propext`,
`Classical.choice`, and `Quot.sound`, with no `sorryAx`.
