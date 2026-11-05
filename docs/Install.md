# Install and first run

shrunk is a small Rust crate and CLI. It has no runtime dependencies.

## Prerequisites

- A Rust toolchain with Cargo (edition 2021). Install it from
  [rustup.rs](https://rustup.rs/) if you do not have one.
- No C compiler, database server, or external library is required.

## Build from source

```sh
git clone https://github.com/vee1e/shrunk.git
cd shrunk
cargo build --release
```

The binary is written to `target/release/shrunk`. To put it on your `PATH`:

```sh
cargo install --path .
```

## Run the demo

```sh
./target/release/shrunk demo
```

Output from this machine (Linux, release build, synthetic data):

```text
seeded 3 series x 200000 points with Counter in 76 ms; file 1.4 MiB

series                   points   blocks      stored         raw    ratio  bits/pt  codecs
cpu.load                 200000      196     1.3 MiB     3.1 MiB     2.3x    56.05  gorilla-rle 195b/1394179B, predictor 1b/7184B
requests.total           200000      196    64.2 KiB     3.1 MiB    48.7x     2.63  predictor 196b/65698B
thermostat.setpoint      200000      196    14.7 KiB     3.1 MiB   212.0x     0.60  rle 196b/15092B
total                    600000              1.4 MiB     9.2 MiB     6.5x

avg(cpu.load) over all points = 45.1299 (196 of 196 blocks read from metadata)
query cpu.load over 300 s: 300 points returned
blocks: 196 total, 2 visited, 2 decoded, 194 skipped (194 time, 0 value)
read: 2048 points from 14130 bytes of compressed payload
query whole series with value > 90 (limit 10): 10 spike points returned
blocks: 196 total, 88 visited, 10 decoded, 78 skipped (0 time, 78 value), 108 left unread after limit
read: 10240 points from 70686 bytes of compressed payload
count over the same 300 s = Some(300.0) (0 of 196 blocks answered from metadata)
```

The count query covers 300 inclusive timestamps.

## Run a benchmark

One million noisy gauge points (one run; timings vary by a few percent):

```sh
./target/release/shrunk bench --points 1000000 --pattern noise
```

```text
ingest        190.3 ms  (5.25M points/s)
save          256.3 ms  (7.8 MiB on disk)
open            1.2 ms
scan all      144.7 ms  (1000000 points returned)
avg all         0.0 ms  (977 blocks answered from metadata)
filtered        1.5 ms  (959 points, 967 blocks skipped of 977)
ratio 2.0x (65 bits/point)
```

The noisy gauge is the hard case. Counters and constants do much better because
they match the predictor, delta, and RLE codecs. Numbers vary with the data; the
benchmark commands are repeatable.

## Next steps

- [Architecture](Architecture.md): how blocks, codecs, and query pruning work.
- [CLI, library, and on-disk format](Info.md): commands, the Rust API, and the
  v3 file layout.
- [Testing and benchmarks](Testing.md): the test suite and reproducible
  compression benchmarks.
