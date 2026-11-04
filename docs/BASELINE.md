# Baseline (pre-hardening)

Recorded before any changes to the storage format or codecs.

Environment:

- Linux Mint 22.3, x86_64, 8 logical CPUs, 7.6 GiB RAM
- rustc/cargo 1.99.1, `--release` (`opt-level=3`, LTO, `panic=abort`)
- `shrunk bench --points 1000000 --pattern <pattern>` (block size 1024)
- Peak RSS from `/usr/bin/time -f %M`; wall time includes ingest, save, open,
  full scan, aggregate and filtered query in one process.

| pattern | ingest ms | save ms | open ms | scan ms | filtered ms | file size | ratio | bits/point | peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| cpu      | 147.7 | 728.4 | 47.7 | 136.2 | 0.3 | 6.8 MiB | 2.2x | 57.00 | 55.5 MiB |
| counter  |  83.5 | 749.3 |  8.8 |  36.2 | 0.4 | 1.5 MiB | 10.1x | 12.72 | 40.8 MiB |
| constant |  52.4 | 775.9 |  0.6 |   8.3 | 0.0 | 69.7 KiB | 224.3x | 0.57 | 34.1 MiB |
| noise    | 175.3 | 955.3 | 53.4 | 165.5 | 1.5 | 7.9 MiB | 1.9x | 66.07 | 73.5 MiB |
| saw      | 154.5 | 1007.8 | 43.6 | 130.5 | 1.4 | 6.7 MiB | 2.3x | 56.23 | 55.5 MiB |

Ingest throughput ranged from 5.70M points/s (noise) to 19.10M points/s
(constant). Full-scan throughput ranged from 6.04M points/s (noise) to
120.5M points/s (constant). The 1% window + `value > 90` query skipped 967 of
977 blocks in every pattern (the filtered query is cheap because it reads
block metadata, not because it scans quickly).

Baseline test suite: 17 unit + 6 integration + 1 doc test, all passing.

`tests/fixtures/v1.shrunk` is a checked-in legacy `SHRUNK01` database
(300 cpu points, block size 64) used to test v1 migration.
