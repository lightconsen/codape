# Test Coverage

Code coverage is measured with [`cargo-llvm-cov`]. Run:

```bash
scripts/coverage.sh
```

The script runs `cargo llvm-cov` over the workspace and writes three artifacts
under `target/coverage/` (or the directory passed as the last argument):

- `lcov.info` — raw line coverage.
- `coverage.tsv` — per-crate hit/total lines.
- `summary.md` — the Markdown table below.

`scripts/coverage.sh --summary-only` regenerates the tables from an existing
`lcov.info` without rerunning the suite.

The tests run through `cargo llvm-cov nextest` when `cargo-nextest` is
installed, and fall back to `cargo test` otherwise. This is not cosmetic:
nextest gives every test binary a process group of its own, while `cargo test`
runs all of them inside one group shared with the caller. A test that signals a
child's process group, or that replaces `PATH` for the whole process, is
confined to its own binary under nextest; under `cargo test` it can reach the
rest of the run. CI installs nextest for this job so coverage measures the same
suite, under the same isolation, as the `Test` job.

Test-only code under `crates/*/tests/`, `benches/` and `examples/` is excluded
from the denominator. Unit-test code inside `src/` is still instrumented (and
counted); treat the numbers as a relative signal, not an absolute one.

## Targets

Advisory per-crate targets: **80% lines** for `core`, `cli` and `tools`, and
**65% lines** for the other workspace crates. The script prints a warning for
every crate below its target but always exits 0, so coverage does not block
local work.

CI runs the same script on every push and pull request (job `Coverage`):

- the workspace total is compared with the baseline recorded in this file, and
  the job fails when it drops more than `COVERAGE_DRIFT` (0.5 points) below
  that baseline. The floor therefore moves with the baseline instead of being
  a fixed number: with a fixed `COVERAGE_MIN` (85%) the project could drift
  down 2.4 points below the 87.4% baseline without CI saying anything. The
  same `COVERAGE_MIN` remains as a backstop for when the baseline line cannot
  be read, and it never raises the floor above `baseline − drift`;
- the `lcov.info` and the summary are uploaded as the `coverage-lcov`
  artifact;
- on a pull request from this repository, a sticky comment reports the total,
  the baseline and the delta, with the per-crate table.

Update the baseline below whenever the figure moves, so the delta on the next
pull request is meaningful.

## Baseline — 2026-11-07

Workspace total: **120,008 / 137,361 lines = 87.4%**.

| crate | lines hit | lines | coverage |
|---|---:|---:|---:|
| agents | 2352 | 2600 | 90.5% |
| artifacts | 3116 | 3426 | 91.0% |
| bridge | 5098 | 5514 | 92.5% |
| cli | 17086 | 23068 | 74.1% |
| commands | 3992 | 4473 | 89.2% |
| config | 2602 | 2816 | 92.4% |
| core | 25891 | 28858 | 89.7% |
| events | 980 | 1060 | 92.5% |
| hooks | 3800 | 4124 | 92.1% |
| insights | 794 | 825 | 96.2% |
| mcp | 8179 | 9277 | 88.2% |
| memory | 4133 | 4391 | 94.1% |
| mods | 3147 | 3465 | 90.8% |
| other (evals and root sources) | 1212 | 1500 | 80.8% |
| plugins | 8765 | 10090 | 86.9% |
| server | 2168 | 2538 | 85.4% |
| telemetry | 4094 | 4322 | 94.7% |
| test-utils | 431 | 713 | 60.4% |
| tools | 13232 | 14755 | 89.7% |
| ui | 6887 | 7355 | 93.6% |
| workflow | 2049 | 2191 | 93.5% |

Below target at baseline:

- **`cli` 74.1%.** Two test-support binaries counted in the crate
  (`mock_anthropic_server.rs`, `static_anthropic_server.rs`) have no coverage of
  their own; excluding them raises the crate further. The remaining gap is
  concentrated in `repl.rs`, `orchestrator.rs`, `interactive_e2e.rs` and
  `tmux.rs`.
- **`test-utils` 60.4%.** Test infrastructure rather than product code; it is
  exercised indirectly by the suites that use it, which line coverage of its
  own tests does not measure.

Other low-coverage source modules (≥100 lines):

| file | coverage |
|---|---:|
| `tools/src/builtins/chrome/client.rs` | 4.1% |
| `tools/src/builtins/lsp_tool.rs` | 32.9% |
| `core/src/wire_adapter.rs` | 39.8% |
| `tools/src/builtins/web_search_tool.rs` | 52.4% |
| `core/src/wire_transport.rs` | 57.3% |
| `core/src/worktree.rs` | 64.2% |
| `cli/src/interactive_e2e.rs` | 68.6% |
| `cli/src/repl.rs` | 72.2% |
| `tools/src/tool.rs` | 72.8% |
| `commands/src/advanced_commands.rs` | 72.9% |

Coverage is not a correctness signal by itself. The safety-critical modules
(`permissions`, `permission_engine`, sandbox, hooks and compaction) are also
targeted by mutation testing and fuzzing; see the testing backlog.

[`cargo-llvm-cov`]: https://github.com/taiki-e/cargo-llvm-cov
