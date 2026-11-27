# External benchmarks

Runners that measure Codape on three public, Docker-based agent benchmarks:

| Benchmark | What it measures | Harness pinned here |
|---|---|---|
| [SWE-bench](https://www.swebench.com/) | Fixing real GitHub issues in Python repositories (Verified, Lite) and in nine languages (Multilingual). Each instance is graded by the project's own tests: `FAIL_TO_PASS` must start passing and `PASS_TO_PASS` must keep passing. | `swebench==5.0.2`, a pinned dataset revision |
| [Terminal-Bench](https://www.tbench.ai/) | Completing end-to-end tasks in a Linux terminal container (files, processes, services, scripts). Each task is graded by task-specific tests. | `terminal-bench==0.2.18`, `terminal-bench-core==0.1.1` (recorded runs); `harbor==0.27.0` with `terminal-bench/terminal-bench@4.1.0` for a current row |
| [ProgramBench](https://programbench.com/) | Rebuilding a program from a compiled reference executable and its documentation, with hidden behavioural tests. The agent must produce source and a `compile.sh`; the grader deletes the reference and builds the submission. | `programbench==1.2.5`, cleanroom `task_cleanroom_v6` |

All runners use the official harness to grade. Codape only generates the work
being graded; the runners do not reimplement test parsing.

These are research instruments, **not CI checks**. See [Why this is not in CI](#why-this-is-not-in-ci).

## Contents

```
scripts/external-benchmarks/
├── README.md
├── check_reporting.py      # transcript parsing, no Docker: the one part CI can run
├── fixture_patches.py      # rebuilds the Terminal-Bench grading fixtures, no Docker
├── swebench/
│   ├── run.sh              # thin wrapper; --help/--dry-run need only stdlib
│   └── run_swebench.py     # per-instance containers, patch extraction, official eval
├── terminal-bench/
│   ├── run.sh              # thin wrapper
│   ├── run_terminal_bench.py
│   ├── codape_agent.py    # Terminal-Bench installed-agent adapter
│   └── codape-setup.sh.j2 # in-container Codape installer
└── programbench/
    ├── run.sh              # thin wrapper; --dry-run needs only stdlib
    └── run_programbench.py # cleanroom container, allowlisted proxy, official eval
```

```bash
python3 -I scripts/external-benchmarks/check_reporting.py
```

Every number a runner prints comes from parsing Codape's stream-json
transcript, and that parsing is the part that rots silently: it is exercised
only by a run that costs Docker, hours and paid tokens, so a runner that read the
wrong frame went unnoticed for months. This script feeds the same functions
synthetic `result` frames — several continuation segments, both cache spellings,
a binary that reports nothing — and runs in CI (`Benchmark reporting`). Change a
runner's reporting and run it first.

It also drives the terminal-bench adapter's **reader**, not just the parser the
reader feeds. That distinction is the whole point of one of the checks: the
parser is correct on any text it is handed, so a check that only calls it passes
however little text the container was asked to return. The adapter used to read
a fixed `tail -c 262144` window, and the protocol writes one frame per streamed
delta — about 160 bytes per handful of characters — so a verbose answer is
roughly 125 KiB and two of them pushed a run's first `result` frame out of the
window. On a 1.1 MiB transcript with two continuation segments the tail saw one
frame, and the run reported 20 turns instead of 30 and 2 000 ms instead of
3 000. It now greps the result frames out of the whole transcript, which is
also a smaller transfer than the tail was.

`fixture_patches.py` is the other half of the same problem. Terminal-Bench
grading is only meaningful against a dataset whose fixtures have been repaired,
and those repairs used to live in a hand-built `0.1.1-analysis` copy that could
not be rebuilt. The script now applies every mechanical one — fourteen anchors
in ten files, chiefly `patch -N` in the four `swe-bench` grading scripts and the
missing `p/255` scaling the two `pytorch-model-cli` tasks need — from the
pristine `0.1.1` cache. Each edit names an anchor and a marker that must not
already be present; an anchor that is missing or ambiguous raises instead of
silently half-patching. `docs/terminal-bench.md` lists the rest as prose
rewrites.

```bash
python3 -I scripts/external-benchmarks/fixture_patches.py build    # edit a copy
python3 -I scripts/external-benchmarks/fixture_patches.py verify   # check it
python3 -I scripts/external-benchmarks/fixture_patches.py self-test
```

`self-test` runs in CI (`Benchmark fixtures`) and needs no Docker: it builds a
synthetic dataset, applies the edits twice, and asserts that a drifted or
ambiguous anchor raises. It also re-establishes *why* `-N` is there by forking a
PTY and showing that an unpatched `patch` prompts for a reverse, which is the
only context the harness runs it in.

## Prerequisites

- Docker with a running daemon (Docker Desktop, OrbStack, Colima, or Linux
  Docker). The official SWE-bench images are `linux/amd64`; on Apple Silicon they
  run under emulation and are slower.
- Python 3.10+ for SWE-bench, Python 3.12+ for Terminal-Bench and ProgramBench.
  `uv` is used when present; otherwise the runner creates a venv with
  `python -m venv`.
- Network access. All runners download a pinned dataset and the benchmark
  harness; SWE-bench pulls one container image per instance, Terminal-Bench
  builds one image per task, and ProgramBench pulls a multi-gigabyte cleanroom
  image (the `cmatrix` image is 4.19 GB). On this Mac,
  `HTTPS_PROXY=http://127.0.0.1:1087` is needed
  for direct downloads; Docker's own proxy settings control image pulls.
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
  spends much of a task compacting instead of working. The runners forward
  `CODAPE_MAX_TOKENS` for the same reason: a reasoning model can otherwise
  spend the whole per-request ceiling on thinking.

## Version pins

Change these only on purpose: they define what a recorded score means.

| Pin | Value | Where |
|---|---|---|
| SWE-bench harness | `5.0.2` | `swebench/run_swebench.py` |
| SWE-bench Verified revision | `78f471bf655a3137b2e8a75af1501690ec009ec3` | same |
| SWE-bench Lite revision | `b0dde1093fe417d83b7184254edf8199c1f0dff5` | same |
| SWE-bench Multilingual revision | `846e647b9f33c0b51b739d005d13d85493c9af09` | same |
| Codape in SWE-bench containers | `v0.2.2`, `x86_64-unknown-linux-musl` | same |
| Terminal-Bench harness | `0.2.18` | `terminal-bench/run_terminal_bench.py` |
| Terminal-Bench dataset | `terminal-bench-core==0.1.1` | same |
| Codape in Terminal-Bench containers | `v0.2.2` | same and `codape_agent.py` |
| ProgramBench harness | `1.2.5` | `programbench/run_programbench.py` |
| ProgramBench cleanroom tag | `task_cleanroom_v6` | same |
| Codape in ProgramBench containers | `v0.2.2`, `x86_64-unknown-linux-musl` | same |

The Codape release downloaded inside containers is verified against the
release's `SHA256SUMS`; a missing entry or checksum mismatch aborts that
instance rather than running an unverified binary.

`v0.2.2` is the oldest pin that reports what a run spent. Before it, a `result`
frame carried `duration_ms` and `num_turns` but no `usage`, `total_cost_usd`,
`model_ms` or `tool_ms` — so the runners could report how long a task took and
not one thing about where the time went. Any score recorded against `v0.2.0`
predates that, which is why the pins moved together.

It also still publishes `codape-*` assets: `v0.1.3` and earlier ship
`coderex-*`, while the installers ask for `codape-<target>.tar.gz`, so pinning
one of those fails with a 404 at container setup — before any model call, which
is why it went unnoticed until a run was actually attempted.

## Quick start: verify locally, then smoke-test

`--dry-run` needs no API key, no venv, and no downloads. If Docker is absent it
prints a warning and continues:

```bash
scripts/external-benchmarks/swebench/run.sh --dry-run --limit 1
scripts/external-benchmarks/terminal-bench/run.sh --dry-run --limit 1
scripts/external-benchmarks/programbench/run.sh --dry-run
```

A one-instance SWE-bench run (expect hundreds of megabytes of image plus paid
model tokens):

```bash
export ANTHROPIC_API_KEY=...
export ANTHROPIC_MODEL=claude-sonnet-4-5-20250929

scripts/external-benchmarks/swebench/run.sh --dataset verified --limit 1
# or a named, exactly reproducible instance
scripts/external-benchmarks/swebench/run.sh --instance astropy__astropy-12907
# non-Python: C, C++, Go, Java, JS/TS, PHP, Ruby, Rust (and Python)
scripts/external-benchmarks/swebench/run.sh --dataset multilingual --limit 1
```

A one-task Terminal-Bench run:

```bash
scripts/external-benchmarks/terminal-bench/run.sh --limit 1
# or by task ID, once you know one from the downloaded dataset
scripts/external-benchmarks/terminal-bench/run.sh --task <task-id>
```

A one-instance ProgramBench run (one cleanroom image, then the official
behavioural evaluator):

```bash
scripts/external-benchmarks/programbench/run.sh
# a specific instance
scripts/external-benchmarks/programbench/run.sh \
  --instance abishekvashok__cmatrix.5c082c6 \
  --timeout 7200 --max-turns 500
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
5. Extracts the patch by resetting the index to `<base_commit>` (a mixed reset;
   the worktree is untouched), staging with `git -c core.fileMode=false add -A`,
   and running `git diff --cached --binary <base_commit>`. Untracked files
   appear as additions, binary changes are included, and commits the agent
   might make are still captured. `.codape` state is removed first. Both
   settings matter: several SWE-bench images check out every file as executable
   and leave that mode in the index, so without the reset and the add setting a
   one-hunk fix carries thousands of mode-only entries.
6. Appends a prediction line with `instance_id`, `model_name_or_path`, and
   `model_patch` to the predictions JSONL.
7. Runs `swebench.harness.run_evaluation` with the pinned local Parquet, the
   same instance IDs, and the predictions file, then reads the official report
   and prints `pass@1 = resolved_instances / total_instances` plus elapsed
   time.

### Why there is no non-Python spec table

Older `swebench` releases kept a `MAP_REPO_VERSION_TO_SPECS` table in the
harness: per repository and version, the install/test commands and log parser.
`swebench==5.0.2` is data-driven instead. Every dataset row carries its own
`eval_script`, `log_parser`, `eval_type`, and `image`, and `make_test_spec`
reads them directly, so Multilingual needs no new build recipes in this runner.
What the runner still owns is a language table
(`MULTILINGUAL_LANGUAGE_BY_PARSER`): it labels each row for reporting and
rejects a pinned revision whose parser it does not recognize. Before spending a
model call, `validate_instances` also checks that every selected row has an
image, base commit, problem statement, evaluation script, and a log parser
implemented by the pinned harness.

Subset selection is deterministic: `--instance` keeps the order given, and
`--limit N` sorts by `instance_id` and takes the first N. Re-running the same
flags on the pinned revision selects the same instances.

Useful options:

| Option | Meaning |
|---|---|
| `--dataset verified\|lite\|multilingual` | Verified (500), Lite (300), or Multilingual (300 across nine languages); default Verified |
| `--limit N` | First N instances after sorting by ID |
| `--instance ID` | Repeatable; exact subset, order preserved |
| `--timeout SEC` | Per-instance Codape timeout (default 1800) |
| `--eval-timeout SEC` | Per-instance test timeout for the official harness |
| `--predictions PATH` | Where to write/append predictions |
| `--eval-only PATH` | Grade an existing predictions file; no model calls |
| `--resume` | Keep existing predictions and skip instances whose every attempt is done |
| `--n-concurrent N` | Instances generated at once (default 1). Independent containers, so this is pure throughput; each holds an image and a model stream |
| `--n-attempts N` | Attempts per instance (default 1). Each attempt gets its own predictions file (`predictions.attempt-N.jsonl`) and its own evaluation under `<run-id>-aN`, because the official harness keys a file by `instance_id`; `pass@1` is then resolved trials / total trials, and `summary.json` prints the trial count beside it |
| `--codape-binary PATH` | Copy a locally built **Linux ELF** binary into containers |
| `--codape-arg ARG` | Repeatable extra Codape argument |
| `--keep-containers` | Leave agent containers for debugging |
| `--dry-run` | Print every command without executing |

`--dry-run` prints the pinned dataset URL and cache path, the venv bootstrap,
the Docker pull/run/install/extract commands, the exact `docker exec` invocation
(values of key variables are not printed), and the official evaluation command.

## Terminal-Bench adapters and runner

There are **two** adapters, because the harness moved and left the old one
behind:

| Adapter | Harness | Dataset | Status |
|---|---|---|---|
| `terminal-bench/codape_agent.py` | `terminal-bench==0.2.18` | `terminal-bench-core==0.1.1` | works; harness frozen |
| `terminal-bench/codape_harbor_agent.py` | `harbor==0.27.0` | `terminal-bench/terminal-bench@4.1.0` | new; needed for a current row |

`terminal-bench==0.2.18` is the last release of the old harness — it is not
removed, and `run_terminal_bench.py` still drives it against the pinned
dataset — but its own README points new users at Harbor, and Harbor is what
Terminal-Bench 4.1 is published against (`v4.1.0`, 2026-11-03, 66 tasks). The
two adapters share no base class, so they are separate files; deleting the old
one would strand the runs recorded in
[terminal-bench.md](../../docs/terminal-bench.md).

### Running an arm: `run-harbor.sh`

`run_terminal_bench.py` drives the old harness only. There was no runner for a
4.1 arm: every recorded 4.1 score came from a shell script on a host that has
since been shut down, and that script was never in the repository. It showed —
its local-binary variable was spelled `CODPE_LOCAL_BINARY`, one letter short, so
the branch could not fire however it was called, and nothing in review would
ever have looked at it.

`terminal-bench/run-harbor.sh` is that script, in the repository:

```bash
export RUN_HARBOR_KEY_FILE=~/.codape-env.json   # mode 0600, see below

# the ten tasks the 2026-11-26 comparison was clean on, on the new release
scripts/external-benchmarks/terminal-bench/run-harbor.sh \
    codape tasks-clean10.txt --concurrent 2 --version v0.2.3

# the same arm on a binary built from the working tree
scripts/external-benchmarks/terminal-bench/run-harbor.sh \
    codape tasks-control9.txt --binary ~/codape/target/x86_64-unknown-linux-musl/release/codape

# what would run, without spending anything
scripts/external-benchmarks/terminal-bench/run-harbor.sh \
    codape tasks-clean10.txt --dry-run
```

It is a wrapper, not a second implementation: it builds the Harbor argv, puts
the adapter's directory on `PYTHONPATH`, and `exec`s harbor so a signal reaches
the trial. What it enforces that a hand-typed command does not:

- **The task lists ship with it.** `tasks-control9.txt`, `tasks-clean10.txt`
  and `tasks-capability2.txt` are the sets the recorded comparisons are built
  on, each with a comment saying which experiment it belongs to. A re-run that
  changes a list is not the same experiment, and passing the list as a file is
  what makes that visible in a shell history.
- **`--version` and `--binary` cannot both reach the adapter.** The adapter
  keeps an explicit version beside `binary_path` — it reads that as the caller
  naming the build — so a command carrying both stamps `agent_info.version`
  with a release the trial did not run. That is the defect the adapters were
  just fixed for, so the script drops `--version` and says so.
- **The model is checked before a container starts.** A model the gateway does
  not serve is rejected mid-trial as `Generation.ModelNotInGo`, which costs a
  container and about twenty minutes to discover. `ALLOWED_MODELS` is one line
  at the top of the script.
- **Credentials never reach a command line.** The key file is a flat JSON object
  read by the script's own shell with `python3 -I`, `shlex.quote`d, and
  `eval`'d; only names are echoed, and only when one is missing. `--dry-run`
  prints the command without the environment.
- **The Claude arm gets the same setup multiplier.** `--agent-setup-timeout-
  multiplier 6` applied to the Codape arm only in the sweep, which is why five
  `AgentSetupTimeoutError` trials sit in the Claude column
  ([tb-harbor-4.1.md](../../docs/tb-harbor-4.1.md)). That was a configuration
  asymmetry, not a model property, so both arms get it here.

`prices.json` is the price table `--pricing` uploads; it lived outside the
repository and is what turns a Codape trial's missing `cost_usd` into a number.

Harbor's CLI takes `--agent module:Class`, where the module must be importable
(a file path fails with `No module named '/abs/x.py'`). Run from this directory,
or put it on `PYTHONPATH`:

```bash
harbor run -d terminal-bench/terminal-bench@4.1.0 \
  --agent codape_harbor_agent:CodapeHarborAgent \
  --model anthropic/claude-sonnet-4-5 \
  -k 4
```

`@4.1.0` is a **release ref**, not a `sha256:` dataset version. `harbor datasets
list` prints versions as digests and will not show `4.1.0` among them; the ref
is resolved by `harbor run -d` itself. Getting this backwards is how an earlier
version of this document concluded, wrongly, that no 4.x dataset existed.

The Harbor adapter's ATIF conversion is checked in CI by
`scripts/external-benchmarks/check_harbor_agent.py`, which runs the same
function on synthetic transcripts with harbor stubbed out. Under a venv that
has harbor installed, the same script additionally validates the result through
harbor's own pydantic models, and drives the MCP mapping through
`harbor.models.task.config.MCPServerConfig` — worth doing before a paid run,
because `AgentCapabilities(atif=True)` is a promise that `trajectory.json` is
valid and `mcp_servers=True` is a promise that the servers reach the agent.

The adapter also declares a `ModelConnectionSpec`, and that is not decoration:
Harbor raises `Agent 'codape' does not support inference connection overrides`
for `--base-url` whenever the spec is missing, and
`Trial._network_plan` reads `model_connection.inference_base_url` to add the
inference host to an allowlist. Two consequences worth knowing before a run:

- Harbor resolves `--base-url` / `--api-key-name` into `model_connection.env`,
  keyed by Codape's own variable names (`CODAPE_BASE_URL`,
  `CODAPE_API_KEY` — the spellings Codape checks before the `ANTHROPIC_` ones).
  Nothing applies that dict for a custom agent, so the adapter merges it into
  the exec environment itself.
- `--model anthropic/claude-sonnet-4-5` is Harbor's spelling, not the API's.
  Codape forwards a model id verbatim, so the adapter strips the `anthropic/`
  prefix and forwards the bare id in `CODAPE_MODEL`; a prefix naming any other
  provider is left alone, because a gateway may really serve `vendor/model`.

The CI check asserts both, and both were confirmed by fault injection while the
check was written — reintroducing either defect makes it fail.

### What the adapter hands the agent beyond the flags

Four things were missing in the version the 2026-11-25 sweep ran (`v0.2.2`
binary, adapter as committed then). All four are fixed and each is guarded by a
check that fails when the fix is removed.

- **The task's MCP servers.** Harbor merges a task's `[environment.mcp_servers]`
  into `agent.mcp_servers` and refuses to prepare the trial unless the agent
  claims `capabilities.mcp_servers`; the adapter claimed `atif` and `base_url`
  only, so `medical-claims-processing` — one `playwright` SSE server — died in
  `Trial._validate_agent_capabilities` with "Agent 'codape' does not support MCP
  servers" before any agent code ran. Exactly one of the 60 cached 4.1 task
  packages declares a server at all — `medical-claims-processing`, and it
  declares the same `playwright` entry under both `[environment]` and
  `[verifier.environment]`. (Twelve more carry `mcp_servers = []`, which is not
  a declaration.) So this defect is one task wide, which is exactly why it went
  unnoticed: a 45-task arm loses one task and still looks complete. The adapter
  now writes them, in Codape's `.mcp.json` shape, to
  `/tmp/codape-config/mcp.json` (mode 0600, owned by the agent user) and passes
  `--mcp-config` — not `<cwd>/.mcp.json`, which would be part of the working
  tree a verifier reads. Harbor's own `streamable-http` spelling passes through
  unchanged because `McpTransport` aliases it to `http`.

  Headers are the part that is not identity. Harbor resolves a task's
  `${VAR}`-templated header into `agent.mcp_header_env.env` and its own agents
  write the *variable name* into their config, so the credential travels only
  through the exec environment. Codape sends a header map verbatim
  (`crates/mcp/src/http.rs`) and expands no templates, so this adapter writes
  the resolved value into the file — which is why the file is 0600 and why
  `capabilities.mcp_http_headers` has to be true for that to be honest.

- **A price table.** Codape emits no `total_cost_usd` for a model it has no
  price for, and the built-in table is Claude-only, so the whole Codape arm of
  the sweep recorded no dollars while the Claude arm recorded a figure per
  trial. The refusal is correct — a Sonnet price wearing another model's name
  is worse than no number — but the adapter never supplied the one input that
  resolves it: `CostTracker::new` already loads `pricing.json` from
  `CLAUDE_CONFIG_DIR`, which the adapter already points at
  `/tmp/codape-config`. `--ak pricing_path=./prices.json` now uploads that file.
  It is validated at construction (missing rate, non-JSON, unreadable) so a typo
  fails before a container is built, rather than as a missing field hours later.
  The four rates are per million tokens. For the sweep's gateway they are $5.00
  input, $0.50 cache read and $25.00 output, solved exactly from the Claude
  arm's own ten clean trials and identical to four decimal places on each. That
  solve lumped every cache token at 0.1× input, so it fixes cache *creation* no
  more precisely than "the same as cache read"; the sweep never ran a model that
  wrote a distinguishable cache volume, so a sharper number would be invented
  rather than measured.

- **The task's skills directory.** Same gate, same one-task-wide shape:
  `cumulative-layout-shift` declares `skills_dir =
  "/app/barber-shop-site/.agents/skills"` and the adapter claimed no
  `capabilities.skills`, so the trial died in `_validate_agent_capabilities`
  before starting. The path is not in the task *package* — that directory holds
  a `skills-lock.json` pinning `vercel-labs/agent-browser` and no `.agents/` —
  but the built image does: `agent-browser/SKILL.md`, 3263 bytes. Harbor's gate
  looks at the declaration and never at whether the path is populated.

  Codape's roots are `<config home>/skills`, `<repo>/.codape/skills`, plugins
  and bundled (`SkillLoader::load_all`), none of which is Harbor's. The adapter
  symlinks the task's directory to `<config home>/skills`, inside
  `/tmp/codape-config` so it cannot appear in a verified artifact. A symlink and
  not a copy: the directory form of a skill (`<name>/SKILL.md` plus its
  templates and scripts) is exactly the form the one shipped 4.1 skill uses, and
  copying would keep `SKILL.md` and drop the rest. The link is conditional on
  the path existing, because the gate fires on a declaration alone and a task
  can name a directory its image never populated.

  No new release is needed: v0.2.2 already reads `<config home>/skills`
  (`orchestrator.rs:1749` on the tag), and it was verified against this very
  image — the v0.2.2 binary logs `skills loaded: 1 from user` and announces
  `agent-browser` in the system-prompt advert. The one thing that does not
  survive is `allowed-tools` (hyphen) in the frontmatter, which Codape reads as
  `allowed_tools` (underscore); the skill still loads and is announced, and
  Codape never enforced a per-skill tool list anyway.

- **`--verbose-tool-input`.** Without it a `tool_use` frame carries
  `{"summary": "Running a command"}` (`repl.rs`, `protocol_frame`), so the ATIF
  trajectory a 300-call run produces cannot say which command ran. The flag has
  been in the CLI since **v0.2.1** (`bc0dcc58`) and `run_swebench.py` has always
  passed it; the Harbor adapter did not, so every trajectory from the sweep names
  tools but not their arguments. Now always passed — the frames grow, which is
  the cost the flag exists to let a benchmark pay for.

The MCP defect and the skills defect are the same defect twice, found one arm
apart, so the fifth fix is to stop finding them that way.
`check_harbor_agent.py` now pairs every field of the declared
`AgentCapabilities` with the code that honours it (`capability_coverage`): a
capability claimed with nothing behind it fails, and so does one claimed while
also sitting on the list of capabilities the adapter deliberately does not
support (`resume`, `handoff`, `native_config`, `windows`, …). Harbor has
thirteen fields and validates several of them before any agent code runs, so
the next task that declares something is a red check rather than a trial that
records a `ValueError` and gets read as the model being weak. Each group also
runs through `run_check`, so a raise inside one is reported as that group's
failure instead of aborting the file and hiding every check after it.

The adapter imposes **no wall clock of its own** by default, and that is
deliberate. Harbor bounds `run()` with the task's `[agent] timeout_sec` and
raises `AgentTimeoutError` when it expires, but it hands that number only to the
oracle agent (`Trial._init_agent`), so an installed agent cannot read it. Any
fixed number is therefore wrong in one direction or the other: longer than the
budget and it never fires, shorter and it ends a run Harbor was still willing to
pay for. The adapter used to hardcode 1740 s, and the tasks in the released
`terminal-bench/terminal-bench@4.1.0` declare a 28800 s budget — so it stopped
7.5 hours early on every task inspected and the unfinished work was recorded as
a wrong answer rather than a timeout. `claude_code.py` passes no timeout either.

`--ak timeout_sec=N` sets one anyway, for a graceful ending rather than for
budget reasons: Codape answers SIGTERM with an `interrupted` result frame
carrying what the provider billed, and the adapter then raises Harbor's
`AgentTimeoutError` so the trial is graded as a timeout. Without that raise a
truncated run returns as the agent's answer and counts against the score.

One thing the trajectory deliberately does not carry: per-step `Step.metrics`.
Codape's stream-json reports usage only on the closing `result` frame, so a
per-turn split cannot be derived from the transcript without inventing one.
Harbor's `compute_model_usage` therefore returns nothing and
`AgentContext.model_usage` falls back to the totals the adapter sets directly
(`n_input_tokens` including cache, `n_cache_tokens`, `n_output_tokens`,
`cost_usd`) — accurate totals, no per-model breakdown.

### The 0.2.18 adapter

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

Two defaults encode what a run has to survive rather than what is cheap:

- `--max-turns 300`. This is the turn count `docs/benchmark.md` records for the
  published runs, so a runner default below it measured a different protocol.
  The histories show turns past 200 are real work, not runaway loops:
  `play-zork` used 239 turns against a 3 600 s timeout and 442 turns in the
  rerun that solved it.
- `--test-timeout 900`. The dataset declares 20–600 s for grading — 56 of the 80
  tasks declare 60 s — and a test that installs its own dependencies does not
  fit in that on a slow link: `pip install torch` alone is 283 MB. A
  `test_timeout` is a plumbing failure, not a wrong answer, so the default is
  above every declared value and only ever raises one. Pass `--test-timeout 0`
  to grade exactly as the dataset declares (each task's own number), which is
  what a comparison against a published Terminal-Bench score wants.

`--n-concurrent` (default 1) is a throughput knob, not a scoring one: each trial
holds a container, so raise it only as far as RAM allows. `--n-attempts` (default
1) is the opposite — it is the only way to tell a stable verdict from a lucky
one, and the harness averages the attempts into one accuracy. Both exist in the
SWE-bench runner too; there they also drive generation, which is one container
per instance and used to run strictly one at a time.

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

## ProgramBench runner

ProgramBench gives the agent a compiled reference executable and the
documentation that ships with it, then asks for an independent reimplementation.
For each selected instance the runner:

1. Creates the pinned `programbench==1.2.5` venv in the cache directory and
   loads the instance's metadata from the ProgramBench dataset.
2. Pulls `programbench/<instance>:task_cleanroom_v6`, starts a disposable
   container as the unprivileged `agent` user, and copies the pinned Codape
   binary to `/tmp/codape`. The container runs `sleep infinity`, so its
   lifetime is independent of the agent timeout; if it has stopped, the runner
   restarts it before reading the workspace, and a timed-out agent still
   produces a submission.
3. Starts `codape-programbench-proxy` on the internal Docker network
   `codape-programbench-internal`. The proxy accepts only
   `CONNECT opencode.ai:443`; every other destination gets `403 Forbidden`.
   The task container is attached only to that internal network, so the agent
   can reach the model endpoint but not the internet. The proxy itself is also
   attached to Docker's `bridge` network for its outbound leg.
4. Runs Codape with `-p --dangerously-skip-permissions
   --no-session-persistence --no-telemetry --output-format stream-json
   --max-turns N`, reads the instruction from a file, and writes
   `/tmp/codape-stream.jsonl` inside the container.
5. Tars `/workspace` as the submission, excluding `./executable`. The reference
   binary is execute-only (`---x--x--x`) so the agent cannot read or copy it,
   and the grader removes it before compiling anyway; including it would make
   the tar itself fail with `EACCES`.
6. Runs `programbench eval <run-dir>` with one worker, then `programbench info`
   so the printed score uses the task's ignored-test and ignored-branch
   filters. The raw counters are printed as `raw_passed` / `raw_total`; the
   `info` output is the official view.

`--instance` selects one ProgramBench instance; the default is
`abishekvashok__cmatrix.5c082c6` (`cmatrix`, C, easy). The pinned dataset
exposes 201 instances (107 Rust, 46 Go, 33 C, 12 C++, and 3 others; 35 carry no
difficulty label). The `cmatrix` cleanroom image has gcc, make, cmake, python3,
cargo/rustc, git, curl, wget, and ncurses headers, but no package-registry
access. Codape's instruction tells the agent to check the
toolchain and not rely on downloads; the proxy enforces the network half.

The allowlist is deliberately narrow. If `ANTHROPIC_BASE_URL` points at a host
other than `opencode.ai`, change `PROXY_ALLOWED_HOST` in the runner; otherwise
the agent's model calls are refused by the proxy. This is what keeps the
ProgramBench inference rule — no looking the program up — from being only a
sentence in the prompt.

Options:

| Option | Meaning |
|---|---|
| `--instance ID` | ProgramBench instance (default `abishekvashok__cmatrix.5c082c6`) |
| `--run-id ID` | Output directory name (default `pb-YYYYmmdd-HHMMSS`) |
| `--timeout SEC` | Agent timeout (default 5 400) |
| `--max-turns N` | Codape turn limit (default 400) |
| `--cpus N` | Container CPU limit, also passed to the evaluator (default 8) |
| `--memory SIZE` | Container memory limit (default `8g`) |
| `--model ID` | Model passed to Codape |
| `--keep-container` | Leave the task container for debugging |
| `--dry-run` | Print the instance, image, proxy policy, and output path |

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

ProgramBench, under
`target/external-benchmarks/programbench/<run-id>/<instance-id>/`:
`codape-stream.jsonl`, `submission.tar.gz`, `eval.log`, the official
`<instance>.eval.json`, and the evaluator's build/test artifacts. `target/` is
gitignored.

## Cost, contamination, and interpretation

- **Architecture.** SWE-bench pins its official images to `linux/amd64`, and
  ProgramBench's cleanroom images are also `linux/amd64`, so the x86_64 Codape
  release matches (under emulation on Apple Silicon). Terminal-Bench builds
  task images for the host architecture. On an arm64 Mac those are
  `linux/arm64`, and Codape has no published aarch64 Linux release yet: use an
  x86_64 host, or build Codape for aarch64 and pass it with `--codape-binary`.
  The runner warns about this in `--dry-run`, and the installer fails with the
  same explanation instead of an exec-format error.
- **Cost.** SWE-bench images commonly occupy 1–5 GB each; a 20-instance run can
  pull tens of gigabytes before evaluation containers start. Model calls are
  paid and unbounded by default: bound them with `--max-turns` and `--timeout`,
  and use a cheap model for plumbing checks. Terminal-Bench builds an image per
  task and also needs network inside the container to download Codape.
  ProgramBench pulls a multi-gigabyte cleanroom image (4.19 GB for `cmatrix`)
  and its evaluator may build additional test images.
- **Token/cost/timing reporting.** From `v0.2.2` a `result` frame carries
  `usage` (billed input and output, plus both cache spellings),
  `total_cost_usd` when the model has a known price, `model_ms` and `tool_ms`
  for that submit, and `duration_ms`/`num_turns`. Codape writes one frame per
  continuation segment, and the two families are counted differently:
  `duration_ms`/`num_turns`/`model_ms`/`tool_ms` describe their own submit and
  are **summed** (reading the last frame alone understates a compacted run),
  while `usage`/`total_cost_usd` are *session* totals at the moment the frame
  was written and the **last** one carrying them is the whole run. This is what
  turns "Codape is slower" into "2 312 s waiting on the provider, 187 s in
  tools". `duration_ms` also covers context building, hooks, compaction and the
  gaps between turns, and that remainder is not small: measured against the
  **local** static provider a two-turn tool run spent 14 ms on the provider,
  33 ms in the tool and 41 ms in neither — the remainder dominated only because
  a local provider answers in milliseconds. Against a real provider the
  provider term grows to seconds; the remainder is the term to read before
  concluding the tools are slow. The SWE-bench summary prints it as `other` and
  `summary.json` carries it as `timing.unattributed_ms`, so a reader comparing
  two of the three does not mistake a minority for the whole. A binary older
  than `v0.2.2` reports none of it, and the runners then
  print `unavailable` rather than inventing numbers.
- **Scored vs. pass@1.** Terminal-Bench reports both: `accuracy` is
  resolved/attempted, the harness definition, while `scored` excludes trials
  whose `failure_mode` is a plumbing failure (`test_timeout`, `parse_error`,
  `agent_installation_failed`) — a grading test that never produced a verdict is
  not a verdict on the agent. Read `scored` when asking what the agent did and
  `accuracy` when comparing with a published number.
- **Contamination.** All three benchmarks are public. Their instructions and,
  for SWE-bench, the reference patches are on the internet and may be present
  in model training data. SWE-bench Verified is human-validated and
  Terminal-Bench tasks are recent, but neither proves the model has not seen the
  task. ProgramBench's rule against looking up the program is enforced by the
  network allowlist in the runner, not only by the prompt. A model can still
  reproduce a memorized fix. Treat scores as upper bounds on generalization.
- **Subset scores.** A 1-instance result is 0 or 1; a 3-instance result is only
  slightly less coarse. Both validate the pipeline, not capability. Report the
  instance IDs and run ID with any number.
- **Non-determinism.** Even with the same pins and subset, model sampling and
  provider behavior vary. Re-run the same named subset for variance — pass
  `--n-attempts N` to either runner to get N attempts per instance in one run,
  since reading a run-to-run difference out of two separate runs confounds it
  with everything else that changed — and do not compare runs with different
  model IDs, harness versions, or dataset revisions.
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

The reporting check above is the exception, and only because it is a different
kind of thing: it does not run a benchmark, it runs the functions that read a
benchmark's output, against fixtures. That is deterministic, key-free, and takes
a second. `fixture_patches.py self-test` is the second exception for the same
reason: it edits synthetic files, not a dataset.

## What was verified locally

- `bash -n` and `shellcheck` on both wrappers; `python -m py_compile` on all
  four Python files.
- `python3 -I scripts/external-benchmarks/check_reporting.py` on every change to
  a runner's reporting, and once against the code it replaced: there it fails
  with five named gaps (no `result_frames`, no `model_ms`/`tool_ms`, a runner
  that could not take the run directory, and a ProgramBench line that reported
  the *last* frame's usage rather than the run's).
- `python3 -I scripts/external-benchmarks/fixture_patches.py self-test`, and
  `verify` against the pristine `0.1.1` cache, where it correctly reports that
  none of the fourteen edits are present. Two rounds of that test found real
  bugs in the matcher: a *prefix* of an anchor (GiB vs MiB) matched, and an
  insertion edit's anchor survives its own application, so "the old text is
  gone" could not signal applied-ness. That is why every edit names a marker.
- `--help`, `--dry-run`, and argument validation (`--instance` together with
  `--limit`, missing binaries, non-ELF binaries, bad run IDs, `--extra-arg`
  containing `=`).
- Pure helpers: deterministic subset selection, prompt construction, predictions
  parsing, report lookup, usage/result-frame parsing, patch extraction
  (untracked, binary, and agent-committed changes all captured and applicable).
- Patch extraction against a real image: the requests image records executable
  mode on files in both the worktree and the index, so resetting the index to
  the base commit and running `git add`/`git diff` with `core.fileMode=false`
  extracted a 308-byte comment-only change as a 308-byte patch with zero
  mode-only entries. Without the reset, the same clean tree already produced 118
  mode-only entries.
- The official `swebench==5.0.2` loader and `make_test_spec` were run against
  the pinned Verified Parquet, and the official predictions loader accepted this
  runner's JSONL. `--limit 1` on that revision selects
  `astropy__astropy-12907`, matching the README example.
- The pinned Multilingual Parquet: `make_test_spec` builds all 300 rows, the 19
  `log_parser` values it uses are all in the harness's `PARSER_REGISTRY`, and
  `validate_instances` passes for Verified (500), Lite (300), and Multilingual
  (300). `--dataset multilingual --dry-run --limit 1` prints the nine
  languages.
- The one-instance Multilingual pipeline check `jqlang__jq-2235` (C,
  `parse_log_jq`) resolved 1/1 on 2026-11-10:
  `multilingual-jq2235-20261110-123314`, generation 740.6 s, evaluation
  41.4 s. This tests the data-driven multilingual path, not the other eight
  languages.
- The Terminal-Bench adapter was imported and instantiated against the real
  `terminal-bench==0.2.18` package: factory import, Jinja setup rendering for
  both release and local-binary modes, command construction, env quoting, and
  result-frame parsing.
- The Harbor adapter resolves through `harbor==0.27.0`'s
  `AgentFactory.get_agent_class_from_config` as `codape_harbor_agent:CodapeHarborAgent`,
  reports no unimplemented abstract methods, and produces an ATIF-v1.8
  trajectory that round-trips through harbor's own pydantic models
  (`Trajectory.model_validate_json`, `extra="forbid"` on every model). Its
  conversion is covered in CI by `check_harbor_agent.py`, which stubs harbor
  when it is absent.

  Against the real package, the connection path was exercised too: with
  `--model anthropic/claude-sonnet-4-5`, `CODAPE_MODEL` resolves to the bare
  `claude-sonnet-4-5` (the prefixed spelling is Harbor's and Codape forwards a
  model id verbatim); `base_url=` is accepted and lands in `CODAPE_BASE_URL`,
  which Codape reads before `ANTHROPIC_BASE_URL`; `MODEL_CONNECTION` makes
  `inference_base_url` non-empty, which is what lets `Trial._network_plan`
  allowlist the inference host; and a key selected by Harbor is projected into
  `CODAPE_API_KEY` and reaches the exec environment. The adapter also
  constructs under harbor, defaults to no wall clock, and accepts
  `--ak timeout_sec=` as the string the CLI actually passes.

  **Nothing beyond that is verified:** this adapter has not run a benchmark
  task, so `install`/`run` against a real container image — the release
  download, the checksum, the stream capture, and the metrics read-back — are
  untested.
- The first attempt to run one did pay for itself, and is why `install` now
  calls `ensure_system_dependencies` first: `cargo-flight-dispatch`, whose base
  is `python:3.13-slim-bookworm`, carries **no `curl`**, so the release step
  exited 127 and the trial failed as `NonZeroAgentExitCodeError` before any task
  work began. `check_harbor_agent.py` now asserts the prerequisites are ensured
  before the download runs, so that specific defect cannot come back. A second
  attempt died at `curl: (18) HTTP/2 stream 1 was not closed cleanly` partway
  through the 20 MB archive, with no retry to absorb it — the download now
  passes `--retry 5 --retry-all-errors`, also guarded.
- With both fixes, `--install-only` on `terminal-bench/cargo-flight-dispatch`
  completes with **0 exceptions** (`jobs/2026-11-24__13-15-54`, 9m29s): the
  image starts, Harbor installs the download prerequisites, the archive
  downloads, its sha256 matches `SHA256SUMS`, the binary installs to
  `/usr/local/bin/codape`, and `configure` writes the per-run settings. That is
  the half a paid run cannot tell you cheaply, because a failure here looks like
  an agent that never started. What is still unverified: `run` against a real
  model — the instruction render, the stream capture, the `result` frame parse,
  and the metrics read-back.
- Two host-level requirements, both hit here: Docker Hub was reached through a
  mirror that refuses `harborframework/*` digests (docker.io direct and
  `mirror.gcr.io` both serve them), and this machine's container egress is
  ~50 KB/s, which pushes the binary download past Harbor's default 360 s agent
  setup budget (`--agent-setup-timeout-multiplier` raises it). Neither is an
  adapter defect, but both make a run on such a host fail in a way that reads
  like one.
- The deadline claim was checked against the released dataset rather than from
  reading one task. `harbor datasets download terminal-bench/terminal-bench@4.1.0`
  declares 66 tasks in its `dataset.toml` and a clean download wrote 63 of them
  (three did not come down over a 50 KB/s link); every one of the 63, and the
  three read from the tag's own `task.toml`, declares
  `[agent] timeout_sec = 28800.0`. The dataset repository's `main` branch has a
  different, mixed set of budgets — 600 s to 12000 s — and it is *not* what
  `@4.1.0` resolves to; an earlier draft of this file quoted those numbers.
- The 80-task `terminal-bench-core==0.1.1` set that `docs/benchmark.md` records
  scores against shares **zero** task names with the 4.1 set, so no recorded
  Terminal-Bench score in this repository is a 4.1 score.
- A real headless Codape invocation with the source build confirmed that `-p`
  reads stdin, accepts the flags used here, and returns a `result` frame; with
  an unreachable API it returned `is_error: true` with exit status 0, which is
  why both the exit status and the frame are checked.
- End-to-end SWE-bench generation and official grading on Linux against the
  pinned images: `psf__requests-1142` on its own (1/1 resolved) and the
  three-repository subset `pallets__flask-5014`, `pytest-dev__pytest-6202`,
  `pylint-dev__pylint-6528` (3/3 resolved), both on 2026-11-10.
- The named batches on the same date: Verified
  `django__django-14007`, `astropy__astropy-14369`, `mwaskom__seaborn-3187`
  (2/3 resolved; `verified-batch1-20261110-122725`), and Lite
  `psf__requests-1963`, `pylint-dev__pylint-5859`,
  `pytest-dev__pytest-11143` (2/3 resolved;
  `lite-batch1-20261110-122725`). The unresolved astropy instance failed its
  own FAIL_TO_PASS/PASS_TO_PASS cases; the unresolved requests instance passed
  all seven FAIL_TO_PASS cases but two PASS_TO_PASS cases hit
  `CERTIFICATE_VERIFY_FAILED` on live HTTPS endpoints. Neither batch had empty
  patches or error instances. Run IDs, timings, patch sizes, and the reasons
  these are pipeline checks rather than scores are recorded in
  [`docs/benchmark.md`](../../docs/benchmark.md#swe-bench-run-history).
- Two zero-token corrected-grammar replays of the Verified batch's unresolved
  instance, both resolving 1/1 with 3/3 FAIL_TO_PASS and 732/732 PASS_TO_PASS:
  `verified-astropy14369-grammarfix-20261110-140245` (12 764 bytes) and a
  best-practice variant with a left-recursive denominator, grammar comments,
  and a regenerated parser table
  (`verified-astropy14369-grammarfix-best-20261110-141456`, 13 121 bytes).
  The best-practice variant also passes all 738 tests in `test_format.py` and
  leaves the wider units suite at the base failure count (17 pre-existing
  environment failures; 2 974 passed base versus 2 980 with the test patch).
  This confirms the original miss was an agent reasoning/verification failure,
  not a task that cannot be implemented.
- A comment-only baseline on `psf__requests-1963`
  (`lite-requests1963-comment-baseline-20261110-131751`) reproduced the same
  two `CERTIFICATE_VERIFY_FAILED` PASS_TO_PASS failures with no agent behavior
  change, confirming they come from the pinned image's trust store or network
  path rather than from the agent's patch.

Not verified: the 12-repository subset (an estimated 40–60 GB of images), the
full 500-instance Verified set, the full 300-instance Lite set, or the full
300-instance Multilingual set. The runs above are named pipeline checks, not
capability scores.
