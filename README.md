# shrunk

shrunk stores time-stamped numbers in less space and answers questions without
unpacking every reading.

## The problem, without the jargon

Picture this. A small greenhouse has 100 temperature and humidity sensors. Each
sensor sends a reading every 10 seconds. That is 864,000 readings a day, or 315
million readings a year. Every reading is just a timestamp and a number.

Stored the usual way, that is about 5 GB a year for one greenhouse. Two
greenhouses over five years is 50 GB of tiny numbers. Backups get slower, disks
fill up, and a simple question like "what was the temperature last Tuesday at
2:15?" makes the software walk through millions of readings to find a few
hundred.

The readings themselves are small. The cost of keeping and searching them is
not.

shrunk changes how the readings are stored:

- Readings are grouped into blocks. The default is 1024 readings, about 3 hours
  of data at one reading every 10 seconds.
- Each block is compressed with whichever codec makes that block smallest. A
  flat thermostat uses a different codec than a noisy sensor or an oscillating
  waveform.
- Each block gets a label: its time range, its lowest and highest value, how
  many readings it holds, and their sum.
- Queries use labels to skip blocks. Totals come from the labels, so a question
  like "what was the average?" does not need to unpack every reading.

On 600,000 synthetic readings from three sensors:

```text
9.2 MiB of raw readings  ->  1.4 MiB stored  (6.5x smaller)

noisy gauge       2.3x smaller
counter          48.7x smaller
constant value  212.0x smaller
```

A 5-minute query read 2 blocks out of 196. The other 194 stayed compressed.

## How a query avoids work

Think of each block as a sealed box of readings with a label on the lid:

```text
box 42
time:   14:00 to 16:49
count:  1024 readings
lowest: 18.2
highest: 24.9
sum:    22,431.7
```

Now ask "did the temperature ever go above 23?"

Any box whose label says `highest: 22.1` cannot contain a match. Leave it
sealed. Only open boxes whose label says the highest value is at least 23.

Ask "what was the average?" Add the sums printed on the labels and divide by
the counts. No box needs to be opened.

That is the whole trick. The compression makes the file smaller. The labels
make the searches smaller.

## Why not just zip the file?

Zip compresses a whole file into one stream. To read one page, you have to
unpack the stream.

shrunk compresses small independent blocks and keeps a label on each one. It
can unpack one block without touching the other 195. A zip file answers "what is
on page 400?" by unpacking everything up to page 400. shrunk answers by reading
the label on the box.

## What shrunk is not

- It is not a cloud service. It is one file and one local process.
- It is not a replacement for Prometheus, InfluxDB, or TimescaleDB. There is no
  replication, clustering, retention policy, or SQL.
- It is not lossless magic for every data set. Noisy floating point sensors get
  about 2x. Integer counters and flat signals get 10x to 200x.
- It does not load every payload on open. `open` reads metadata only and reads a
  block's compressed bytes when a query needs them.
- It is a small project meant to be read, run, and extended.

## Documentation

- [Install and first run](docs/Install.md): build, run the demo, and benchmark.
- [Architecture](docs/Architecture.md): blocks, codecs, headers, and the query
  path.
- [CLI, library, and on-disk format](docs/Info.md): commands, the Rust API, the
  v3 file layout, and the write-ahead log.
- [Testing and benchmarks](docs/Testing.md): test commands, coverage, and
  reproducible compression benchmarks.
- [Compression notes](docs/COMPRESSION.md): measured codec results, research
  verdicts, and trade-offs.
- [Pre-hardening baseline](docs/BASELINE.md): the numbers this work started
  from.
- [Contributing](CONTRIBUTING.md): development setup, the pull request process,
  and the pull request template.

## Layout

```text
src/bits.rs      bit writer/reader
src/block.rs     block construction, metadata, codec selection, lazy payloads
src/codec.rs     gorilla, delta-varint, rle, gorilla-rle, dolphin, predictor, raw
src/crc.rs       CRC-32
src/db.rs        series, writes, queries, aggregates, format versions, checkpoints
src/gen.rs       deterministic synthetic data
src/wal.rs       write-ahead log, replay, truncation
src/main.rs      CLI
tests/           integration tests and legacy fixtures
docs/            install, architecture, CLI/format, testing, compression notes
CONTRIBUTING.md  contribution guide and PR checklist
```

## Limits

These are deliberate for a small project:

- One writer at a time. The WAL makes appends durable, but there is no
  multi-writer coordination or transaction API beyond per-batch atomicity.
- A checkpoint rewrites the whole snapshot. Incremental appends avoid that cost
  until you checkpoint, but the snapshot itself is still a full rewrite.
- `open` reads metadata only, but the block index for every series is held in
  memory. The payloads are read lazily.
- No deletes, retention windows, downsampling, or rollups.
- No multi-series joins or expression language. One series per query, one
  comparison per value filter.
- Gorilla's advantage depends on data. Noisy f64 gauges get about 2x. Integer
  counters and flat signals get 10x to 200x.
- A 58-byte block overhead means very small series can occupy more space than raw
  points. Use a larger block size for many tiny series.
- Aggregates ignore NaN in sum, avg, min, and max. Count counts every point.
- The predictor codec allocates two small tables per encoded/decoded block.
  That is fine at the default block size but is a future optimization target.
