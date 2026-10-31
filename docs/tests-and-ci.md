# Tests and CI

## Tests

```sh
cargo test --workspace
```

The NATS conformance and end-to-end tests need a `nats-server` binary. Set
`MOLT_NATS_SERVER=/path/to/nats-server` or put it on `PATH`
(`go install github.com/nats-io/nats-server/v2@v2.15.1`, using the Go version
in `.github/ci-versions.toml`). Without it they print
a notice and pass; CI always runs them. Set `MOLT_REQUIRE_NATS_TESTS=1`, as CI
does, to make a missing server fail those tests instead, and
`MOLT_REQUIRE_SANDBOX_TESTS=1` to do the same for a sandbox that does not start,
so a skipped test is never counted as one that passed.

CI runs the tests with `RUST_TEST_THREADS=4`, fixed rather than taken from the
runner, and prints the host's process, open-file and namespace limits before the
tests and again after a failure. A sandbox test that cannot start its sandbox for
want of a resource says which, with what the host had (see
[Linux command isolation](sandbox.md#commands-at-once-and-a-host-that-runs-short)).

The Rust release and components are pinned in `rust-toolchain.toml`, and CI
reads that file too. Go and nats-server are pinned in `.github/ci-versions.toml`;
Go cannot fetch a newer toolchain automatically. NATS binaries use an exact
version/OS/architecture/Go cache key and are version-checked on every run,
including cache hits. Actions are pinned to commit SHAs. Toolchain upgrades
belong in a separate PR so ordinary changes keep the same tool versions when
rerun. The runner image and apt's isolation backend still receive OS updates;
these pins do not claim a bit-for-bit identical machine image.

`crates/molt-bench/tests/tasks.rs` validates every benchmark task, which takes
`python3`, `node` (20 or newer), `go` and `cargo`. A missing toolchain skips
its tasks with a notice, except on CI.

## From an issue to a PR in CI

An opt-in composite GitHub Action runs a maintainer-labelled issue with a fixed
budget, then creates a PR containing check and baseline evidence, known cost
and run ID. It requires a trusted installed Molt build and a working sandbox;
fork PR events cannot start a secret-bearing run. See
[scripts/ci/README.md](../scripts/ci/README.md) for setup, permissions and limits.
