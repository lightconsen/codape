# External benchmarks

Runners that measure CodeRex on two public, Docker-based agent benchmarks:

| Benchmark | What it measures | Harness pinned here |
|---|---|---|
| [SWE-bench](https://www.swebench.com/) | Fixing real GitHub issues in Python repositories. Each instance is graded by the project's own tests: `FAIL_TO_PASS` must start passing and `PASS_TO_PASS` must keep passing. | `swebench==5.0.2`, SWE-bench Verified (or Lite) at a pinned dataset revision |
| [Terminal-Bench](https://www.tbench.ai/) | Completing end-to-end tasks in a Linux terminal container (files, processes, services, scripts). Each task is graded by task-specific tests. | `terminal-bench==0.2.18`, `terminal-bench-core==0.1.1` |

Both runners use the official harness to grade. CodeRex only generates the work
being graded; the runners do not reimplement test parsing.

These are research instruments, **not CI checks**. See [Why this is not in CI](#why-this-is-not-in-ci).

## Contents

```
scripts/external-benchmarks/
├── README.md
├── swebench/
│   ├── run.sh              # thin wrapper; --help/--dry-run need only stdlib
│   └── run_swebench.py     # per-instance containers, patch extraction, official eval
└── terminal-bench/
    ├── run.sh              # thin wrapper
    ├── run_terminal_bench.py
    ├── coderex_agent.py    # Terminal-Bench installed-agent adapter
    └── coderex-setup.sh.j2 # in-container CodeRex installer
```

## Prerequisites

- Docker with a running daemon (Docker Desktop, OrbStack, Colima, or Linux
  Docker). The official SWE-bench images are `linux/amd64`; on Apple Silicon they
  run under emulation and are slower.
- Python 3.10+ for SWE-bench, Python 3.12+ for Terminal-Bench. `uv` is used when
  present; otherwise the runner creates a venv with `python -m venv`.
- Network access. Both runners download a pinned dataset and the benchmark
  harness; SWE-bench pulls one container image per instance, and Terminal-Bench
  builds one image per task. On this Mac, `HTTPS_PROXY=http://127.0.0.1:1087` is
  needed for direct downloads; Docker's own proxy settings control image pulls.
- Disk: budget several gigabytes for the venvs and small runs, and tens of
  gigabytes for a meaningful SWE-bench subset.
- A model endpoint. CodeRex is configured only through environment variables:

  ```bash
  export ANTHROPIC_API_KEY=...                     # or CODEREX_API_KEY / OPENAI_API_KEY
  export ANTHROPIC_BASE_URL=https://...            # optional, for a gateway
  export ANTHROPIC_MODEL=claude-sonnet-4-5-20250929
  ```

  `CODEREX_*` variables take precedence over `ANTHROPIC_*`, which take
  precedence over `OPENAI_*` (`CODEREX_MODEL`, `CODEREX_BASE_URL`,
  `CODEREX_API_KEY`, and the matching `ANTHROPIC_*` / `OPENAI_*` names). Keys are
  read from the environment, forwarded to containers by variable *name* only
  (`docker exec -e ANTHROPIC_API_KEY`), and never written to a repository file.

## Version pins

Change these only on purpose: they define what a recorded score means.

| Pin | Value | Where |
|---|---|---|
| SWE-bench harness | `5.0.2` | `swebench/run_swebench.py` |
| SWE-bench Verified revision | `78f471bf655a3137b2e8a75af1501690ec009ec3` | same |
| SWE-bench Lite revision | `b0dde1093fe417d83b7184254edf8199c1f0dff5` | same |
| CodeRex in SWE-bench containers | `v0.1.3`, `x86_64-unknown-linux-musl` | same |
| Terminal-Bench harness | `0.2.18` | `terminal-bench/run_terminal_bench.py` |
| Terminal-Bench dataset | `terminal-bench-core==0.1.1` | same |
| CodeRex in Terminal-Bench containers | `v0.1.3` | same and `coderex_agent.py` |

The CodeRex release downloaded inside containers is verified against the
release's `SHA256SUMS`; a missing entry or checksum mismatch aborts that
instance rather than running an unverified binary.

## Quick start: verify locally, then smoke-test

`--dry-run` needs no API key, no venv, and no downloads. If Docker is absent it
prints a warning and continues:

```bash
scripts/external-benchmarks/swebench/run.sh --dry-run --limit 1
scripts/external-benchmarks/terminal-bench/run.sh --dry-run --limit 1
```

A one-instance SWE-bench run (expect hundreds of megabytes of image plus paid
model tokens):

```bash
export ANTHROPIC_API_KEY=...
export ANTHROPIC_MODEL=claude-sonnet-4-5-20250929

scripts/external-benchmarks/swebench/run.sh --dataset verified --limit 1
# or a named, exactly reproducible instance
scripts/external-benchmarks/swebench/run.sh --instance astropy__astropy-12907
```

A one-task Terminal-Bench run:

```bash
scripts/external-benchmarks/terminal-bench/run.sh --limit 1
# or by task ID, once you know one from the downloaded dataset
scripts/external-benchmarks/terminal-bench/run.sh --task <task-id>
```

Re-grade an existing predictions file without spending model tokens:

```bash
scripts/external-benchmarks/swebench/run.sh --eval-only target/external-benchmarks/swebench/<run>/predictions.jsonl
```

Both wrappers pass `--help` through.

## SWE-bench runner

For each selected instance the runner:

1. Downloads the pinned dataset Parquet once into
   `$CODEREX_BENCH_CACHE` (default
   `~/.cache/coderex-external-benchmarks`, revision-addressed).
2. Pulls the instance's official `swebench/sweb.eval...` image and starts a
   throwaway container at `/testbed`.
3. Installs the pinned CodeRex release (or copies `--coderex-binary`) and seeds
   an isolated `CLAUDE_CONFIG_DIR` with telemetry disabled, so no first-run
   prompt blocks the run.
4. Sends the issue text on stdin to
   `coderex -p --dangerously-skip-permissions --no-session-persistence
   --no-telemetry --output-format stream-json --max-turns N`, bounded by
   `timeout`.
5. Extracts the patch with `git add -A` and
   `git diff --cached --binary <base_commit>`, so untracked files appear as
   additions, binary changes are included, and commits the agent might make are
   still captured. `.coderex` state is removed first.
6. Appends a prediction line with `instance_id`, `model_name_or_path`, and
   `model_patch` to the predictions JSONL.
7. Runs `swebench.harness.run_evaluation` with the pinned local Parquet, the
   same instance IDs, and the predictions file, then reads the official report
   and prints `pass@1 = resolved_instances / total_instances` plus elapsed
   time.

Subset selection is deterministic: `--instance` keeps the order given, and
`--limit N` sorts by `instance_id` and takes the first N. Re-running the same
flags on the pinned revision selects the same instances.

Useful options:

| Option | Meaning |
|---|---|
| `--dataset verified\|lite` | Verified (500) or Lite (300); default Verified |
| `--limit N` | First N instances after sorting by ID |
| `--instance ID` | Repeatable; exact subset, order preserved |
| `--timeout SEC` | Per-instance CodeRex timeout (default 1800) |
| `--eval-timeout SEC` | Per-instance test timeout for the official harness |
| `--predictions PATH` | Where to write/append predictions |
| `--eval-only PATH` | Grade an existing predictions file; no model calls |
| `--resume` | Keep existing predictions and skip completed IDs |
| `--coderex-binary PATH` | Copy a locally built **Linux ELF** binary into containers |
| `--coderex-arg ARG` | Repeatable extra CodeRex argument |
| `--keep-containers` | Leave agent containers for debugging |
| `--dry-run` | Print every command without executing |

`--dry-run` prints the pinned dataset URL and cache path, the venv bootstrap,
the Docker pull/run/install/extract commands, the exact `docker exec` invocation
(values of key variables are not printed), and the official evaluation command.

## Terminal-Bench adapter and runner

`coderex_agent.py` subclasses `AbstractInstalledAgent`, the supported extension
point in `terminal-bench==0.2.18`. The harness:

1. Copies `coderex-setup.sh.j2` into the task container and sources it.
2. The setup script downloads the pinned musl release, verifies SHA256SUMS,
   installs `/usr/local/bin/coderex`, and writes a telemetry-disabled settings
   file under `/tmp/coderex-config`.
3. `perform_task` copies the task instruction into the container as a file
   (`/installed-agent/instruction.txt`) rather than typing it into tmux;
   newlines in an instruction would otherwise press Enter mid-prompt.
4. It runs `coderex -p --dangerously-skip-permissions
   --no-session-persistence --no-telemetry --output-format stream-json
   --max-turns N < /installed-agent/instruction.txt`, records the exit status,
   and leaves the container in whatever state the agent produced. The task's
   tests determine the score.
5. The adapter reads CodeRex's result frame for error status and token usage,
   when present.

Options mirror the SWE-bench runner: `--limit`, repeatable `--task`,
`--timeout`, `--max-turns`, `--model`, `--coderex-version`,
`--coderex-binary`, repeatable `--extra-arg`, `--run-id`, `--output-dir`,
`--cache-dir`, and `--dry-run`. The runner bootstraps
`venv-terminal-bench-0.2.18` in the cache directory and runs the pinned `tb`
from it, with `PYTHONPATH` pointing at the adapter directory.

`--limit` maps to `tb --n-tasks`, whose order comes from the pinned dataset
version. For a subset that stays meaningful across dataset revisions, name
tasks with `--task`; those are exact. The adapter assumes the task container
runs as root and can write `/usr/local/bin`, which is the normal
Terminal-Bench setup.

## Outputs

SWE-bench, under `target/external-benchmarks/swebench/<run-id>/`:

```
predictions.jsonl
instances/<instance-id>/{prompt.txt,agent-stream.jsonl,agent-stderr.log,patch.diff,record.json}
eval/reports/<model>.<run-id>.json      # official harness report
eval/evaluation.log
summary.json                            # pass@1, timings, usage, instance list
```

Terminal-Bench, under `target/external-benchmarks/terminal-bench/<run-id>/`:
the harness's task directories, per-trial logs (including the CodeRex
stream-json transcript written through the mounted `/agent-logs` directory),
and aggregate `results.json` (accuracy, resolved/unresolved IDs, failure
modes). `target/` is gitignored.

## Cost, contamination, and interpretation

- **Architecture.** SWE-bench pins its official images to `linux/amd64`, so the
  x86_64 CodeRex release always matches (under emulation on Apple Silicon).
  Terminal-Bench builds task images for the host architecture. On an arm64 Mac
  those are `linux/arm64`, and CodeRex has no published aarch64 Linux release
  yet: use an x86_64 host, or build CodeRex for aarch64 and pass it with
  `--coderex-binary`. The runner warns about this in `--dry-run`, and the
  installer fails with the same explanation instead of an exec-format error.
- **Cost.** SWE-bench images commonly occupy 1–5 GB each; a 20-instance run can
  pull tens of gigabytes before evaluation containers start. Model calls are
  paid and unbounded by default: bound them with `--max-turns` and `--timeout`,
  and use a cheap model for plumbing checks. Terminal-Bench builds an image per
  task and also needs network inside the container to download CodeRex.
- **Token/cost reporting.** CodeRex 0.1.3 exits 0 and emits a `result` frame but
  does **not** populate `usage` or `total_cost_usd` in headless stream-json. Both
  runners therefore print `tokens/cost: unavailable` and `summary.json` records
  `null`; they do not invent numbers. The parsers already read those fields if a
  future CodeRex release emits them. Estimate spend from the provider's billing
  dashboard.
- **Contamination.** Both benchmarks are public. Their instructions and, for
  SWE-bench, the reference patches are on the internet and may be present in
  model training data. SWE-bench Verified is human-validated and Terminal-Bench
  tasks are recent, but neither proves the model has not seen the task. The
  SWE-bench prompt tells the agent not to look up the issue online; a model can
  still reproduce a memorized fix. Treat scores as upper bounds on
  generalization.
- **Subset scores.** A 1-instance result is 0 or 1; it validates the pipeline,
  not capability. Report the instance IDs and run ID with any number.
- **Non-determinism.** Even with the same pins and subset, model sampling and
  provider behavior vary. Re-run the same named subset for variance; do not
  compare runs with different model IDs, harness versions, or dataset
  revisions.
- **Untrusted content.** Issue text and task instructions are data. They can
  contain prompt injection. The runners deliberately run CodeRex with
  `--dangerously-skip-permissions` inside a disposable container, which is the
  only reason that is acceptable; do not reuse these containers for anything
  else.

## Why this is not in CI

Normal CI cannot host these runs:

- They require a Docker daemon with many gigabytes of disk and image pulls.
- They spend real money on model calls, with runtimes measured in hours for a
  useful subset.
- They need outbound network to Hugging Face, GitHub, Docker Hub, and the model
  provider, which is exactly what a hermetic CI check avoids.
- LLM output is non-deterministic, so a red result is not a code regression.

The repository's existing benchmark job (`scripts/test-code-agent-benchmark.sh`)
is intentionally cassette-based and key-free. These external runners are manual
tools for release notes and research; keep them out of required checks.

## What was verified locally

- `bash -n` and `shellcheck` on both wrappers; `python -m py_compile` on all
  three Python files.
- `--help`, `--dry-run`, and argument validation (`--instance` together with
  `--limit`, missing binaries, non-ELF binaries, bad run IDs, `--extra-arg`
  containing `=`).
- Pure helpers: deterministic subset selection, prompt construction, predictions
  parsing, report lookup, usage/result-frame parsing, patch extraction
  (untracked, binary, and agent-committed changes all captured and applicable).
- The official `swebench==5.0.2` loader and `make_test_spec` were run against
  the pinned Verified Parquet, and the official predictions loader accepted this
  runner's JSONL. `--limit 1` on that revision selects
  `astropy__astropy-12907`, matching the README example.
- The Terminal-Bench adapter was imported and instantiated against the real
  `terminal-bench==0.2.18` package: factory import, Jinja setup rendering for
  both release and local-binary modes, command construction, env quoting, and
  result-frame parsing.
- A real headless CodeRex invocation with the source build confirmed that `-p`
  reads stdin, accepts the flags used here, and returns a `result` frame; with
  an unreachable API it returned `is_error: true` with exit status 0, which is
  why both the exit status and the frame are checked.

Not verified: a full benchmark run. That needs Docker, network, an API key, and
paid model calls.
