# External benchmarks

Runners that measure Codape on two public, Docker-based agent benchmarks:

| Benchmark | What it measures | Harness pinned here |
|---|---|---|
| [SWE-bench](https://www.swebench.com/) | Fixing real GitHub issues in Python repositories. Each instance is graded by the project's own tests: `FAIL_TO_PASS` must start passing and `PASS_TO_PASS` must keep passing. | `swebench==5.0.2`, SWE-bench Verified (or Lite) at a pinned dataset revision |
| [Terminal-Bench](https://www.tbench.ai/) | Completing end-to-end tasks in a Linux terminal container (files, processes, services, scripts). Each task is graded by task-specific tests. | `terminal-bench==0.2.18`, `terminal-bench-core==0.1.1` |

Both runners use the official harness to grade. Codape only generates the work
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
    ├── codape_agent.py    # Terminal-Bench installed-agent adapter
    └── codape-setup.sh.j2 # in-container Codape installer
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
- A model endpoint. Codape is configured only through environment variables:

  ```bash
  export ANTHROPIC_API_KEY=...                     # or ANTHROPIC_AUTH_TOKEN,
                                                   # CODAPE_API_KEY / CODAPE_AUTH_TOKEN / OPENAI_API_KEY
  export ANTHROPIC_BASE_URL=https://...            # optional, for a gateway
  export ANTHROPIC_MODEL=claude-sonnet-4-5-20250929
  export CODAPE_CONTEXT_WINDOW=131072              # optional: the endpoint's real window
  export CODAPE_MAX_TOKENS=32000                   # optional: raise the output ceiling
  export CODAPE_CUSTOM_HEADERS='x-example: value'  # optional: one `Name: value` per line
  ```

  `CODAPE_*` variables take precedence over `ANTHROPIC_*`, which take
  precedence over `OPENAI_*` (`CODAPE_MODEL`, `CODAPE_BASE_URL`,
  `CODAPE_API_KEY`, and the matching `ANTHROPIC_*` / `OPENAI_*` names).
  `ANTHROPIC_AUTH_TOKEN` and `CODAPE_AUTH_TOKEN` are accepted in the same slot
  as the API key, matching Codape's own provider precedence. Keys are read from
  the environment, forwarded to containers by variable *name* only
  (`docker exec -e ANTHROPIC_API_KEY`), and never written to a repository file.

  `CODAPE_CUSTOM_HEADERS` and `CODAPE_CONTEXT_WINDOW` are forwarded too. The
  latter matters when the endpoint's real context window is larger than
  Codape's conservative 32K default for an unknown model; without it the agent
  spends much of a task compacting instead of working. Both runners forward
  `CODAPE_MAX_TOKENS` for the same reason: a reasoning model can otherwise
  spend the whole per-request ceiling on thinking.

## Version pins

Change these only on purpose: they define what a recorded score means.

| Pin | Value | Where |
|---|---|---|
| SWE-bench harness | `5.0.2` | `swebench/run_swebench.py` |
| SWE-bench Verified revision | `78f471bf655a3137b2e8a75af1501690ec009ec3` | same |
| SWE-bench Lite revision | `b0dde1093fe417d83b7184254edf8199c1f0dff5` | same |
| Codape in SWE-bench containers | `v0.2.0`, `x86_64-unknown-linux-musl` | same |
| Terminal-Bench harness | `0.2.18` | `terminal-bench/run_terminal_bench.py` |
| Terminal-Bench dataset | `terminal-bench-core==0.1.1` | same |
| Codape in Terminal-Bench containers | `v0.2.0` | same and `codape_agent.py` |

The Codape release downloaded inside containers is verified against the
release's `SHA256SUMS`; a missing entry or checksum mismatch aborts that
instance rather than running an unverified binary.

`v0.2.0` is the oldest pin that works: it is the first release published under
the `codape-*` asset names. `v0.1.3` and earlier still ship `coderex-*`
archives, while the installers ask for `codape-<target>.tar.gz`, so pinning one
of those fails with a 404 at container setup — before any model call, which is
why it went unnoticed until a run was actually attempted.

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
   `$CODAPE_BENCH_CACHE` (default
   `~/.cache/codape-external-benchmarks`, revision-addressed).
2. Pulls the instance's official `swebench/sweb.eval...` image and starts a
   throwaway container at `/testbed`.
3. Installs the pinned Codape release (or copies `--codape-binary`) and seeds
   an isolated `CLAUDE_CONFIG_DIR` with telemetry disabled, so no first-run
   prompt blocks the run.
4. Sends the issue text on stdin to
   `codape -p --dangerously-skip-permissions --no-session-persistence
   --no-telemetry --output-format stream-json --max-turns N`, bounded by
   `timeout`.
5. Extracts the patch with `git add -A` and
   `git diff --cached --binary <base_commit>`, so untracked files appear as
   additions, binary changes are included, and commits the agent might make are
   still captured. `.codape` state is removed first.
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
| `--timeout SEC` | Per-instance Codape timeout (default 1800) |
| `--eval-timeout SEC` | Per-instance test timeout for the official harness |
| `--predictions PATH` | Where to write/append predictions |
| `--eval-only PATH` | Grade an existing predictions file; no model calls |
| `--resume` | Keep existing predictions and skip completed IDs |
| `--codape-binary PATH` | Copy a locally built **Linux ELF** binary into containers |
| `--codape-arg ARG` | Repeatable extra Codape argument |
| `--keep-containers` | Leave agent containers for debugging |
| `--dry-run` | Print every command without executing |

`--dry-run` prints the pinned dataset URL and cache path, the venv bootstrap,
the Docker pull/run/install/extract commands, the exact `docker exec` invocation
(values of key variables are not printed), and the official evaluation command.

## Terminal-Bench adapter and runner

`codape_agent.py` subclasses `AbstractInstalledAgent`, the supported extension
point in `terminal-bench==0.2.18`. The harness:

1. Copies `codape-setup.sh.j2` into the task container and sources it.
2. The setup script downloads the pinned musl release, verifies SHA256SUMS,
   installs `/usr/local/bin/codape`, and writes a telemetry-disabled settings
   file under `/tmp/codape-config`.
3. `perform_task` copies the task instruction into the container as a file
   (`/installed-agent/instruction.txt`) rather than typing it into tmux;
   newlines in an instruction would otherwise press Enter mid-prompt.
4. It runs `codape -p --dangerously-skip-permissions
   --no-session-persistence --no-telemetry --output-format stream-json
   --max-turns N < /installed-agent/instruction.txt`, records the exit status,
   and leaves the container in whatever state the agent produced. The task's
   tests determine the score.
5. The adapter reads Codape's result frame for error status and token usage,
   when present.

Options mirror the SWE-bench runner: `--limit`, repeatable `--task`,
`--timeout`, `--max-turns`, `--model`, `--codape-version`,
`--codape-binary`, repeatable `--extra-arg`, `--extra-instructions`,
`--run-id`, `--output-dir`, `--cache-dir`, and `--dry-run`. The runner
bootstraps `venv-terminal-bench-0.2.18` in the cache directory and runs the
pinned `tb` from it, with `PYTHONPATH` pointing at the adapter directory.

`--extra-instructions` appends a frame to every task instruction — for
example, that the container is the target machine and no user is available.
The adapter also sets `CODAPE_SANDBOX=off` inside the task container: the
container is already the isolation boundary, and Codape's shell sandbox
otherwise denies the network and the privilege-drop syscalls `apt-get` needs.
The SWE-bench runner sets the same switch for its in-container agent; see
`docs/sandbox.md` for the switch itself.

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
the harness's task directories, per-trial logs (including the Codape
stream-json transcript written to the host-mounted `/installed-agent` directory),
and aggregate `results.json` (accuracy, resolved/unresolved IDs, failure
modes). `target/` is gitignored.

## Cost, contamination, and interpretation

- **Architecture.** SWE-bench pins its official images to `linux/amd64`, so the
  x86_64 Codape release always matches (under emulation on Apple Silicon).
  Terminal-Bench builds task images for the host architecture. On an arm64 Mac
  those are `linux/arm64`, and Codape has no published aarch64 Linux release
  yet: use an x86_64 host, or build Codape for aarch64 and pass it with
  `--codape-binary`. The runner warns about this in `--dry-run`, and the
  installer fails with the same explanation instead of an exec-format error.
- **Cost.** SWE-bench images commonly occupy 1–5 GB each; a 20-instance run can
  pull tens of gigabytes before evaluation containers start. Model calls are
  paid and unbounded by default: bound them with `--max-turns` and `--timeout`,
  and use a cheap model for plumbing checks. Terminal-Bench builds an image per
  task and also needs network inside the container to download Codape.
- **Token/cost reporting.** Codape 0.2.0 exits 0 and emits a `result` frame but
  does **not** populate `usage` or `total_cost_usd` in headless stream-json. Both
  runners therefore print `tokens/cost: unavailable` and `summary.json` records
  `null`; they do not invent numbers. The parsers already read those fields if a
  future Codape release emits them. Estimate spend from the provider's billing
  dashboard.
- **Contamination.** Both benchmarks are public. Their instructions and, for
  SWE-bench, the reference patches are on the internet and may be present in
  model training data. SWE-bench Verified is human-validated and Terminal-Bench
  tasks are recent, but neither proves the model has not seen the task. The
  SWE-bench prompt tells the agent not to look up the issue online; a model can
  still reproduce a memorized fix. Treat scores as upper bounds on
  generalization.
- **Subset scores.** A 1-instance result is 0 or 1; a 3-instance result is only
  slightly less coarse. Both validate the pipeline, not capability. Report the
  instance IDs and run ID with any number.
- **Non-determinism.** Even with the same pins and subset, model sampling and
  provider behavior vary. Re-run the same named subset for variance; do not
  compare runs with different model IDs, harness versions, or dataset
  revisions.
- **Untrusted content.** Issue text and task instructions are data. They can
  contain prompt injection. The runners deliberately run Codape with
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
- A real headless Codape invocation with the source build confirmed that `-p`
  reads stdin, accepts the flags used here, and returns a `result` frame; with
  an unreachable API it returned `is_error: true` with exit status 0, which is
  why both the exit status and the frame are checked.
- End-to-end SWE-bench generation and official grading on Linux against the
  pinned images: `psf__requests-1142` on its own (1/1 resolved) and the
  three-repository subset `pallets__flask-5014`, `pytest-dev__pytest-6202`,
  `pylint-dev__pylint-6528` (3/3 resolved), both on 2026-11-10. Run IDs,
  timings, patch sizes, and the reasons these are pipeline checks rather than a
  Verified score are recorded in
  [`docs/benchmark.md`](../../docs/benchmark.md#swe-bench-run-history).

Not verified: the 12-repository subset (an estimated 40–60 GB of images) or the
full 500-instance Verified set. The runs above are named pipeline checks, not
capability scores.
