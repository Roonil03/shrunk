# Contributing to shrunk

Thanks for helping improve shrunk. This guide covers the development workflow,
the checks we expect before a pull request, and the pull request template used by
this repository.

## Ways to contribute

- Report a bug or a confusing error message.
- Propose a feature or a workload that shrunk handles poorly.
- Improve the codecs, query path, durability, or CLI.
- Add tests, fixtures, or reproducible benchmarks.
- Improve the documentation in `README.md` and `docs/`.
- Share a real sensor or metrics trace that can be added as a benchmark.

For anything larger than a small fix, open an issue first so the approach can be
discussed before you spend time on it.

## Before you start

- Read the [README](README.md) for the high-level idea.
- Read [Architecture](docs/Architecture.md) for blocks, codecs, and the query
  path.
- Read [Info](docs/Info.md) for the CLI, the Rust API, and the on-disk format.
- Search existing issues and pull requests to avoid duplicate work.

## Development setup

Prerequisites:

- A Rust toolchain with Cargo (edition 2021). Install it from
  [rustup.rs](https://rustup.rs/).
- No C compiler, database server, or external library is required.

Fork the repository on GitHub, then clone your fork:

```sh
git clone git@github.com:<your-user>/shrunk.git
cd shrunk
git remote add upstream git@github.com:vee1e/shrunk.git
git fetch upstream
```

Build and run the test suite:

```sh
cargo build
cargo test --all-targets
cargo test --doc
```

Run the demo to see the storage and query behaviour end to end:

```sh
cargo run -- demo
```

## Making changes

### Create a focused branch

Branch from the current upstream `main`:

```sh
git switch -c feat/short-description upstream/main
```

Use a short, descriptive name:

- `feat/...` for a new feature
- `fix/...` for a bug fix
- `perf/...` for a performance change
- `docs/...` for documentation
- `test/...` for tests or fixtures
- `chore/...` for tooling or cleanup

Keep one logical change per branch. Small pull requests are easier to review and
safer to merge.

### Follow the existing style

- Run `cargo fmt` before committing.
- Keep public APIs and error messages consistent with the surrounding code.
- Prefer clear code over clever code. This is a small, readable project.
- Do not add runtime dependencies without discussing it first. The crate is
  intentionally dependency-free.

### Add tests

Every behaviour change needs a test. Prefer a small, deterministic test over a
large fixture. Useful places:

- `src/codec.rs`: codec round trips and gating helpers.
- `src/block.rs`: block construction, metadata validation, codec selection.
- `src/db.rs`: writes, queries, aggregates, format migration, checkpoints.
- `src/wal.rs`: WAL records, replay, truncation.
- `tests/integration.rs`: end-to-end behaviour, corruption, crash recovery.
- `tests/fixtures/`: legacy format fixtures.

For codec changes, also add or update a randomized round-trip case so NaN,
infinities, `-0.0`, and negative timestamps stay covered.

### Preserve compatibility

- `FORMAT_VERSION` is the single source of truth. Do not hardcode the format
  version or magic elsewhere; derive them from it.
- Codec IDs are persisted in block headers and are **never renumbered**. Add a
  new ID instead of changing an existing one.
- Keep the v1 and v2 decoders working. Saving may upgrade a legacy file, but
  opening it must not fail.
- Preserve the public Rust API where possible. If a change is unavoidable,
  document it in the pull request and update the README and docs.
- Keep the format change and the codec change separate when you can. That makes
  review and rollback easier.

### Update the documentation

Documentation lives in two places:

- `README.md` is the short overview: the problem, the query idea, what shrunk is
  not, the layout, and the limits.
- `docs/` holds the detail:
  - `docs/Install.md`: build, demo, and first benchmark.
  - `docs/Architecture.md`: blocks, codecs, headers, and the query path.
  - `docs/Info.md`: CLI, Rust API, on-disk format, and WAL.
  - `docs/Testing.md`: test commands, coverage, and benchmarks.
  - `docs/COMPRESSION.md`: measured codec results and research verdicts.
  - `docs/BASELINE.md`: the pre-hardening baseline.

If you add a new doc, link it from the README documentation list. If you move a
section, update every link that points to it.

### Benchmark codec and performance changes

For a change that claims a size or speed win, include reproducible numbers:

```sh
cargo build --release
./target/release/shrunk codec-bench --points 1000000
./target/release/shrunk segment-bench --points 1000000
./target/release/shrunk bench --points 1000000 --pattern <pattern>
```

Record the command, the pattern, the hardware, the build profile, and the
before/after values. Do not claim a win that the checked-in benchmark cannot
reproduce.

## Commit messages

Use a short imperative subject with a conventional prefix:

```text
feat: add Dolphin-inspired skip codec
fix: reject duplicate timestamps in a batch
perf: gate the predictor with a 128-point sample
docs: split CLI and format details into docs/Info.md
test: cover torn WAL tails
chore: add a pull request template
```

Keep the subject under about 72 characters. Use the body to explain *why* the
change is needed, not just what changed. Reference issues when relevant.

## Pull request process

1. Rebase on the latest upstream `main`.
2. Run the full check suite:
   ```sh
   cargo fmt --check
   cargo test --all-targets
   cargo test --doc
   cargo build --release
   ```
3. Push your branch to your fork:
   ```sh
   git push -u origin feat/short-description
   ```
4. Open a pull request against `vee1e/shrunk:main`.
5. Fill in the repository pull request template (see below). GitHub inserts it
   automatically from `.github/pull_request_template.md`.
6. Link the issue the pull request closes, if there is one.
7. Keep the pull request focused. Move unrelated cleanups to a separate branch.
8. Respond to review comments with code or a short explanation. Re-run the
   checks after every change.

### Pull request template

The repository uses this template. Fill in every section; remove the HTML
comments and any sections that genuinely do not apply.

```markdown
## Summary

<!-- Briefly explain the problem being solved and the approach taken. -->

## Changes

- [ ] Added or updated functionality
- [ ] Added or updated tests
- [ ] Updated documentation (if needed)

## Testing

<!-- Describe the commands run and the results. -->

- [ ] `cargo fmt --check`
- [ ] `cargo test --all-targets`

## Notes

<!-- Anything reviewers should know -->
```

What reviewers look for:

- **Summary**: one or two sentences on the problem and the solution. Mention any
  format, API, or codec-ID change.
- **Changes**: tick the boxes that apply. Call out user-visible behaviour,
  format, or public API changes in the summary or notes even when a box is not
  ticked.
- **Testing**: the exact commands you ran and their results. Add new test names
  or fixtures. For codec or performance changes, include the benchmark command
  and before/after numbers.
- **Notes**: migration concerns, follow-up work, trade-offs, or a pointer to the
  issue. Call out anything that is intentionally not covered.

If a pull request is a draft or not ready for review, say so in the title or use
GitHub's draft pull request feature.

## Reporting bugs

A good bug report includes:

- The shrunk version or commit hash.
- Your operating system, Rust version (`rustc --version`), and build profile.
- The smallest data or command that reproduces the problem.
- What you expected and what happened instead.
- The full error message or panic backtrace.
- Whether the file was written by an older version of shrunk.

If the bug involves corruption, keep a copy of the file and, if possible, add a
minimal failing test in `tests/`. Do not attach private production data; reduce
it to a synthetic fixture first.

## Feature requests

Describe:

- The workload: series count, points per series, value shape (counter, gauge,
  oscillating, noisy), and timestamp regularity.
- The question you want to answer and how often.
- What you tried, including the current file size or query time.
- Why the existing codecs or query path are not enough.

Concrete numbers and a reproducible example are much more useful than a general
request.

## Questions

Open a GitHub issue with the `question` label, or start a discussion if the
repository has discussions enabled. For a small clarification, a pull request
comment is fine.

Be respectful and assume good intent. This is a small project maintained in
spare time; reviews may take a few days.
