# Container and HTTP service

The container runs a real shrunk database with an authenticated JSON HTTP API.
Each successful write is synced to the write-ahead log before the server replies.
The snapshot and WAL live in `/data`; keep that directory on a persistent volume.
One service process owns one database. It is a single-node service without TLS,
replication, SQL, or built-in user accounts.

## Run with Docker

The fork publishes `ghcr.io/roonil03/shrunk` for Linux amd64. The image becomes
available after the Container workflow successfully publishes it from `main`.
For another fork, replace `roonil03/shrunk` with its lowercase owner/repository.
Use `sha-<full-commit>` or an image digest for repeatable deployments; `latest`
tracks the last successful `main` publication.

```sh
export SHRUNK_TOKEN="$(openssl rand -hex 32)"
docker pull ghcr.io/roonil03/shrunk:latest
docker run -d --name shrunk \
  -p 127.0.0.1:8080:8080 \
  -e SHRUNK_TOKEN \
  -v shrunk-data:/data \
  --read-only --cap-drop=ALL --security-opt=no-new-privileges \
  --memory=512m --cpus=2 --pids-limit=64 \
  ghcr.io/roonil03/shrunk:latest
curl --fail http://127.0.0.1:8080/healthz
```

The token is mandatory and must contain 32..4096 printable ASCII characters
without whitespace. Keep it for later requests and restarts. The image runs as
UID/GID `10001:10001` and has no shell. A named volume gets its initial directory
ownership from the image. An existing bind-mounted data directory must already
be writable by that UID/GID (rootless Docker uses mapped host IDs).

The checked-in Compose file includes these runtime limits and persistent storage:

```sh
export SHRUNK_TOKEN="$(openssl rand -hex 32)"
docker compose up -d
docker compose logs shrunk
docker compose down
```

`docker compose down` keeps the named volume. `docker compose down -v` deletes it.
Compose defaults to the published fork image `ghcr.io/roonil03/shrunk:latest`.
Set `SHRUNK_IMAGE` to pin a digest, such as
`ghcr.io/roonil03/shrunk@sha256:<digest>`, or use another publisher's image, such
as `ghcr.io/your-org/shrunk:latest`. Replace placeholders with the desired image
before running Compose:

```sh
export SHRUNK_IMAGE=ghcr.io/your-org/shrunk:latest
docker compose up -d
```

Configure the token in your deployment's secret store so it survives a new shell.
For a file-mounted secret, set `SHRUNK_TOKEN_FILE=/run/secrets/shrunk_token` instead
of `SHRUNK_TOKEN` and mount a readable file at that path. A trailing newline in
the file is ignored. Setting both token variables is an error.

The port is published on host loopback. Put a TLS reverse proxy with connection
and rate limits in front of the service for access from other machines. The
bearer token grants all database operations; do not share it with read-only
clients. There are no CORS headers or browser credential flows.

## Write and query

All `/v1/*` routes require `Authorization: Bearer <token>`. POST bodies must use
`Content-Type: application/json`. Timestamps are signed 64-bit integers in
milliseconds; time bounds are inclusive. Values must be finite JSON numbers.

```sh
curl --fail-with-body http://127.0.0.1:8080/v1/write \
  -H "Authorization: Bearer $SHRUNK_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"series":"temperature","points":[{"ts":1700000000000,"value":21.5},{"ts":1700000001000,"value":22.0}]}'
# {"written":2}

curl --fail-with-body http://127.0.0.1:8080/v1/query \
  -H "Authorization: Bearer $SHRUNK_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"series":"temperature","from":1700000000000,"to":1700000001000,"limit":100,"filter":{"op":"ge","value":22}}'

curl --fail-with-body http://127.0.0.1:8080/v1/aggregate \
  -H "Authorization: Bearer $SHRUNK_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"series":"temperature","agg":"avg"}'

curl --fail-with-body http://127.0.0.1:8080/v1/series \
  -H "Authorization: Bearer $SHRUNK_TOKEN"
```

| Method and path | Body | Result |
| --- | --- | --- |
| `GET /healthz` | None; no authentication | `{"status":"ok"}` after the database opens |
| `POST /v1/write` | `series`, `points: [{ts,value}]` | `{"written":N}`; one atomic durable batch |
| `POST /v1/query` | `series`; optional `from`, `to`, `limit`, `filter` | `points`, `truncated`, compression `stats` |
| `POST /v1/aggregate` | `series`, `agg`; optional `from`, `to`, `filter` | `value` (or `null`), compression `stats` |
| `POST /v1/delete` | `series`; at least one of `from`, `to` | `points_deleted`, `blocks_dropped`, `blocks_rewritten` |
| `POST /v1/delete-series` | `series` | `series_deleted: true`, `points_deleted` |
| `POST /v1/retain` | `cutoff`, or both `now` and `window_ms` | Effective `cutoff` and deletion counters |
| `POST /v1/downsample` | `series`, `interval_ms`, `agg`; optional `from`, `to`, `limit` | Bucket `points`, `truncated`, `next_from`, compression `stats` |
| `POST /v1/rollup` | `series`, `target`, `interval_ms`, `agg`; optional `from`, `to`, `max_buckets` | Target `series`, `written` bucket count |
| `GET /v1/series` | None | Series names, point/block counts, compressed/raw sizes, total points, format version |
| `POST /v1/checkpoint` | Optional empty object | `{"checkpointed":true}` |

`agg` is `count`, `sum`, `avg`, `min`, or `max`. A filter contains `op` (`gt`, `ge`,
`lt`, `le`, `eq`, `ne`) and numeric `value`. Queries default to 1,000 points and
allow at most 100,000. `truncated: true` means more matching points exist. Continue
with `from` set to the last returned timestamp plus one, retaining the same
filter and upper bound. A last timestamp of `9223372036854775807` ends pagination.

Timestamps must strictly increase within each series, including across batches.
A duplicate or out-of-order sample rejects the whole batch with HTTP 409. There
is no overwrite or upsert route. A disconnected client may have committed its
write: query the affected range before retrying. Writes have no idempotency key.

Unknown JSON fields are rejected. Validation errors use HTTP 400, unknown series
404, missing/incorrect tokens 401, oversized bodies 413, incorrect content types
415, and a full work queue 503. API errors use
`{"error":{"code":"...","message":"..."}}`. Routing/method errors may use the
HTTP framework's default response. Server-side I/O failures return 500 and log
details to stderr. There is no request/body/token logging.

The API accepts at most 1 MiB per body, 10,000 points per write, 10,000 series,
and series names of 1..255 UTF-8 bytes without control characters. Body reads
time out after 10 seconds. At most 16 authenticated requests and eight database
operations can be in flight; excess work gets 503. Database operations serialize
on a blocking worker, keeping disk access off the HTTP runtime. These limits do
not impose a disk quota; monitor volume space and memory. Retention is disabled
unless explicitly enabled below.

Existing snapshots created with the CLI/library can be placed in the volume as
`shrunk.db` together with their `shrunk.db.wal`, if present, while the server is
stopped. Non-finite values already present in imported files are returned as
JSON `null`; aggregates follow the library's NaN semantics. See
[the database API and storage format](Info.md).

## Deletion, retention, and rollups

```sh
# At least one range bound is required. Omitted from/to is open-ended.
curl --fail-with-body http://127.0.0.1:8080/v1/delete \
  -H "Authorization: Bearer $SHRUNK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"series":"temperature","to":1700000059999}'

curl --fail-with-body http://127.0.0.1:8080/v1/retain \
  -H "Authorization: Bearer $SHRUNK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"now":1700086400000,"window_ms":86400000}'

curl --fail-with-body http://127.0.0.1:8080/v1/downsample \
  -H "Authorization: Bearer $SHRUNK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"series":"temperature","interval_ms":60000,"agg":"avg","limit":1000}'

curl --fail-with-body http://127.0.0.1:8080/v1/rollup \
  -H "Authorization: Bearer $SHRUNK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"series":"temperature","target":"temperature.minute","interval_ms":60000,"agg":"avg","max_buckets":100000}'
```

Range deletion is inclusive and keeps an empty series name. A body containing
only `series` is rejected; whole-series deletion is explicit through
`/v1/delete-series`. After tail deletion, writes may resume after the last
surviving timestamp. Series deletion permits fresh writes at any timestamp.
Missing series return 404. These operations are serialized with writes and
durable before a successful response.

`/v1/retain` is one-time event-time maintenance over every series, including
materialized targets. It keeps timestamps at or after `cutoff`, or at or after
`now - window_ms` when those two fields are used. The window must be positive;
subtraction saturates at i64::MIN. Future-dated points are kept. The caller
supplies the time; this route does not consult the wall clock or configure the
operational policy below. Repeated retention with the same cutoff is a no-op.

Downsampling groups points into fixed, half-open buckets aligned to timestamp
zero. With width 10, -10..-1 is labeled -10 and 0..9 is labeled 0. Only points
inside the inclusive requested range participate, so edge buckets can be
partial and their timestamps can precede `from`. Empty buckets are omitted;
undefined NaN-only average/min/max buckets are omitted too. The first bucket's
mathematical start saturates at i64::MIN when necessary. Full blocks entirely
inside one bucket use existing header aggregates; other blocks are decoded.

Downsampling defaults to 1,000 returned buckets and permits at most 100,000.
The engine stops after a bounded prefix, including one lookahead bucket; it
does not build an unbounded result and truncate afterward. If `truncated` is
true, continue with `from` set to `next_from`, keeping `to`, `interval_ms`, and
`agg` unchanged. `next_from` is the next bucket boundary, rather than the last
timestamp plus one, which would split a bucket. It is `null` when complete.
Value filters are not accepted for downsampling or rollups.

Rollups create a new, distinct target series using the same automatic lossless
block compression as writes. They leave the source unchanged and do not refresh
automatically. Existing targets, including empty ones, return 409 with
`series_exists`; an empty result still reserves the target name. `max_buckets`
defaults to 100,000 and accepts 1..100,000. Exceeding it returns 413 with
`limit_exceeded` before changing the database; a partial rollup is never saved.
If a client disconnects or receives a server-side I/O error, inspect the target
before retrying. Explicitly delete an existing target before rebuilding it.
Aggregate values do not retain source counts, so later averages of average
rollups are unweighted. Floating-point rounding follows the library aggregates.

## Persistence and backups

The server opens with `Db::open_rw`, holds the exclusive writer lock, and appends
through the fsynced WAL. A second service must not share the same data volume.
Before the next write after the WAL reaches 32 MiB, the server checkpoints it.
An explicit checkpoint uses the same serialized writer path:

```sh
curl --fail-with-body -X POST http://127.0.0.1:8080/v1/checkpoint \
  -H "Authorization: Bearer $SHRUNK_TOKEN"
docker stop --time 60 shrunk
```

SIGTERM and Ctrl-C drain active HTTP requests, then checkpoint before exiting.
Use a sufficient shutdown grace period for the size of your database; Compose
sets 60 seconds. A forced kill may skip that checkpoint, but acknowledged
appends recover from the WAL on restart. Recovery guarantees still depend on
the filesystem and storage honoring sync operations. The default Docker health
probe checks HTTP reachability, not free disk space or the success of a future
write. Inspect write failures and server logs.

For a consistent backup, stop the server and copy the entire data volume,
including both the snapshot and WAL. Restart after the copy. Restore into an
empty volume while the service is stopped, preserving file ownership. Copying
only the snapshot from a live server can omit acknowledged WAL writes. Full
checkpoints rewrite the snapshot, so provide enough disk space for the old and
new files during replacement. Deletes, retention with expired data, and rollups
can write two full snapshots: the old WAL is checkpointed and emptied before
the replacement is published. This prevents deleted tail points resurfacing
during recovery. An I/O error after rename can leave the new state visible;
inspect it before retrying. The library barriers subsequent writes and retries
on directory durability. There is no background compaction policy.

## Optional operational retention

Set `SHRUNK_RETENTION_MS` to a positive i64 duration in milliseconds to enable
scheduled retention. Unset or empty disables it. The service first expires old
points after recovery and before accepting HTTP traffic, then checks every
60 seconds. Set `SHRUNK_RETENTION_INTERVAL_MS` to 1..86,400,000 milliseconds to
change the cadence; setting a cadence without a window is an error. Invalid
configuration fails startup. Compose forwards both variables:

```sh
export SHRUNK_RETENTION_MS=604800000       # seven days
export SHRUNK_RETENTION_INTERVAL_MS=60000 # check every minute
docker compose up -d
```

This policy uses the server's wall clock: each pass removes timestamps strictly
older than `current UTC epoch milliseconds - SHRUNK_RETENTION_MS`. It applies
to all series, including rollup bucket timestamps, while preserving empty series
names and future-dated points. Clock adjustments affect the cutoff. Old data
accepted by a write can remain queryable until the next pass; writes remain
durable and the policy later deletes expired points explicitly.

Policy work shares the serialized database worker. A busy or failed sweep logs
an error and retries at the next interval; missed ticks do not queue a backlog.
No-op checks read the earliest timestamp from metadata without copying database
payloads or rewriting snapshots. When data expires, the same durable retention
path used by HTTP is applied. Shutdown stops the timer, waits for any active
sweep, then performs the final checkpoint. Choose a cadence that allows for
full snapshot maintenance on the expected database size.

## Build and verify locally

```sh
docker build -t shrunk:local .
python3 service/tests/smoke.py --image shrunk:local
```

Or run the service directly with Rust 1.89 or newer and Python 3:

```sh
cargo build --manifest-path service/Cargo.toml --locked
cargo test --manifest-path service/Cargo.toml --all-targets --locked
python3 service/tests/smoke.py --binary service/target/debug/shrunk-server
export SHRUNK_TOKEN="$(openssl rand -hex 32)"
SHRUNK_DATA_DIR=./data cargo run --manifest-path service/Cargo.toml --locked
```

The native server defaults to `127.0.0.1:8080` and `./data`; the image sets
`SHRUNK_LISTEN=0.0.0.0:8080` and `SHRUNK_DATA_DIR=/data`. `SHRUNK_LISTEN` takes a
numeric socket address, including `[::]:8080` for IPv6. The built-in
`shrunk-server healthcheck` subcommand probes the configured address without
requiring a token. Other command arguments are rejected.

The smoke test sends real HTTP requests and verifies authentication, input
limits, atomic conflict rejection, filters, aggregates, concurrent writers,
crash recovery before and after checkpoints, persistence across container
replacement, clean shutdown, the runtime UID, and the health probe. It also
tests range/series deletion, accidental omitted ranges, explicit retention,
bounded bucket pagination, cross-block aggregation, compressed rollup storage,
restart persistence, and startup/scheduled wall-clock expiry. Use
`--scenario base`, `--scenario lifecycle`, or `--scenario retention` to run one
group; the default runs all three on separate data volumes.

## Image publication

The `Container` GitHub Actions workflow builds and runs the image smoke test on
pull requests without publishing. On a push to `main`, a `v*` version tag, or a
manual run with `publish` selected, it publishes to
`ghcr.io/<lowercase-owner>/<lowercase-repository>` using `GITHUB_TOKEN` with
`packages: write`. It tags commits `sha-<full-commit>`, tags version releases
with their semantic version, and tags `main` as `latest`. It then pulls the
published digest and repeats the persistence smoke test. OCI labels link the
package to the repository running the workflow.

GitHub creates a new GHCR package as private, independently of the repository's
visibility. After the first successful publication, the package owner must open
their GitHub profile's **Packages**, select **shrunk**, open **Package settings**,
and use **Change visibility** in the Danger Zone to make it **Public**. Public
GHCR packages support anonymous pulls. Confirm from a machine without registry
credentials before advertising the image as public. See GitHub's
[Container registry guide](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry)
and [package visibility settings](https://docs.github.com/en/packages/learn-github-packages/configuring-a-packages-access-control-and-visibility).

If publication reports permission denied for an existing package, grant this
repository write access under the package's **Manage Actions access** settings.
Private images require an authenticated `docker login ghcr.io` with package read
access. No Docker Hub account or separately stored registry password is needed
for the checked-in GHCR publication workflow.
