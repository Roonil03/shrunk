# Architecture

This page describes how shrunk stores a series and how a query avoids reading
blocks it does not need. For the exact byte layout, see
[Info.md](Info.md#on-disk-format).

## Blocks and codec selection

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

Each block costs 58 bytes for its header and checksums, plus the payload. That
is negligible for a 1024-point block and noticeable for a three-point series.

## Query path

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

The fields are total blocks, blocks visited, blocks decoded, blocks skipped by
time or value bounds, and points read. A `--limit` that stops early reports the
remaining blocks as left unread instead of counting them as time skips.

This is not a general compressed-domain execution engine. Arbitrary arithmetic
on encoded bits is not supported. The compression is queryable through block
pruning and stored aggregates.

## See also

- [Info.md](Info.md): CLI, Rust API, v3 file layout, and WAL.
- [Testing.md](Testing.md): tests and reproducible benchmarks.
- [COMPRESSION.md](COMPRESSION.md): measured codec results and research notes.
