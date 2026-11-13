# Building Codape

Everything about getting a binary out of this repository: prerequisites, the
supported routes, what a full build actually costs, and the parts that fail on
underpowered machines.

Users installing a release do not need any of this — see the install section of
the [README](../README.md) or [self-update.md](self-update.md).

## Prerequisites

| | |
|---|---|
| Rust | **1.97 or newer**; `rust-toolchain.toml` pins 1.99.1 and selects it automatically |
| C compiler | required — QuickJS (`mods`), `ring` and `wasmtime` compile C |
| `pkg-config` | required |
| Linux | also `libdbus-1-dev` and `lld` (`.cargo/config.toml` links `x86_64-unknown-linux-gnu` with `-fuse-ld=lld`) |
| Windows | the MSVC toolchain |
| Optional | `bubblewrap` on Linux for the filesystem sandbox; Landlock is the fallback, and Seatbelt is built in on macOS |

`rust-toolchain.toml` pins the channel rather than following `stable`, because
clippy and rustdoc run with `-D warnings` in CI and a new stable release that
adds a lint would fail a build unrelated to the change under review. Bump it
together with the version named in `.github/workflows/*.yml`.

The 1.97 figure is checked, not just declared: the `MSRV (1.97)` job compiles
the whole workspace on 1.97 and nothing else. It runs no tests — the promise is
that the workspace *compiles* on that toolchain, which is what a consumer
needs.

## Routes

### `scripts/build.sh` (recommended)

Wraps the build and checks the toolchain and platform prerequisites *before*
compiling, so a missing `libdbus-1-dev` fails in seconds instead of at minute
twenty.

```bash
scripts/build.sh                       # release profile, starts with cargo clean
scripts/build.sh --debug               # faster compile, larger binary
scripts/build.sh --workspace           # every crate, not just the default set
scripts/build.sh --no-clean            # keep the build cache
scripts/build.sh --run -- --help       # build, then launch the binary
```

### Plain cargo

```bash
cargo build --release --locked              # the binary: target/release/codape
cargo build --workspace --locked            # every crate including tests' deps
```

`--locked` is not optional in practice: `Cargo.lock` is committed, and CI
builds with it.

### Release archives

The release profile is `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`,
`strip = true`, `panic = "abort"`. It is slow and produces the shipped binary.
`cargo-dist` builds the five published targets —

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`
- `x86_64-pc-windows-msvc`

## What a build costs

The workspace is 20 crates plus the `evals` harness, roughly 281k lines of Rust
across 21 workspace members. Two resource limits matter, and they are not the
same one:

**Memory during linking.** CI caps `CARGO_BUILD_JOBS` at 2 for exactly this
reason and says so in `ci.yml`: *"Linking the whole workspace at once exhausts
the runner's memory"* — on a 16 GB runner. Plan for **8 GB or more**; below
that, a build either needs `CARGO_BUILD_JOBS=1` or swap. Without swap, an
exhausted build dies by OOM killer rather than by an error message, which is
the frustrating failure mode. The per-crate build logic is unaffected: this is
peak linker memory for large binaries, not a per-crate cost.

**Disk.** `target/` for this workspace runs to tens of gigabytes once both a
debug and a release build are present — budget **30 GB** and check free space
before a first full build. `cargo clean` between profiles is the cheap way to
avoid needing that much at once.

`CARGO_INCREMENTAL=0` for a one-shot or CI build saves both memory and disk at
the cost of slower rebuilds; incremental is on by default here for development
(see `.cargo/config.toml`).

## Running the tests

The full suite is roughly 4.3k tests and does not fit a single `nextest` call on
a hosted runner — see the long comment in `.github/workflows/ci.yml` for the
measurement. CI splits it into two invocations, and a local run that hits the
same cutoff should do the same:

```bash
cargo nextest run --workspace --locked --no-fail-fast -E 'not package(codape-cli)'
cargo nextest run -p codape-cli --locked --no-fail-fast
```

`cargo test --workspace --locked` also works and is what a single-crate run
should use. Doctests need their own pass, because `nextest` does not run them:

```bash
cargo test --doc --workspace --locked
```

Some suites need extra setup — the PTY-driven TUI tests need
`source export_env.local.sh`, and the provider-backed E2E suites replay
committed cassettes so they need no key. [CONTRIBUTING.md](../CONTRIBUTING.md)
has the per-suite detail and [verification.md](verification.md) has every check
that is not `cargo test`.

## Related

- [CONTRIBUTING.md](../CONTRIBUTING.md) — workflow, test setup, review
- [verification.md](verification.md) — audit, deny, licences, clippy, rustdoc
- [coverage.md](coverage.md) — `scripts/coverage.sh` and the coverage baseline
- [arch.md](arch.md) — what the crates actually are
