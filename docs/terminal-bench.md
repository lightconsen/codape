# Terminal-Bench: what runs, and what it has found

Terminal-Bench is the external agent benchmark CodeRex is measured against. The
runner lives in [`scripts/external-benchmarks/terminal-bench/`](../scripts/external-benchmarks/terminal-bench/)
and is described there; this file is the **record of results**, and it is the
place to look before blaming a model for a result CodeRex caused, or vice
versa.

The distinction that matters throughout: a benchmark score of 0 can mean the
agent harness failed, *or* it can mean the model chose not to follow the task.
Those have nothing in common except the number, and separating them is why this
record keeps both the frame subtype and the test's own verdict.

## How to read a run

Every trial writes these to
`target/external-benchmarks/terminal-bench/<run-id>/`:

| File | What it answers |
|---|---|
| `results.json` | `accuracy`, per-task `parser_results`, and `failure_mode` |
| `<task>/*/sessions/agent.log` | the last lines of CodeRex's stream-json transcript |
| `<task>/*/sessions/tests.log` | the grading test's real output, including the assertion text |
| `<task>/*/panes/post-agent.txt` | the terminal state when the agent stopped |
| `/logs/coderex-stream.jsonl` (in-container) | the **full** transcript, copied there by the adapter |

`failure_mode` is the first thing to check, because it says whether the agent
ever got a fair test:

| `failure_mode` | Meaning |
|---|---|
| `unset` | the agent exited cleanly and the task's tests ran and scored it |
| `unknown_agent_error` | CodeRex's `result` frame reported an error (see the subtype) |
| `test_timeout` | the grading test itself timed out — the run was not scored on merit |
| `agent_timeout` | CodeRex exceeded its own timeout |

A trial with `failure_mode: unset` and `accuracy: 0` is a **model or prompt
result**. A trial with any other mode is a **plumbing result** and says nothing
about the model until the plumbing is fixed.

## Environment on this machine

- macOS on arm64; Docker runs `linux/arm64` task images.
- There is no published aarch64 CodeRex release, so runs use
  `--coderex-binary` with a locally cross-built binary:

  ```bash
  docker run --rm --platform linux/arm64 \
    -v "$PWD":/src:ro -v /tmp/coderex-arm-target:/target \
    -v /tmp/coderex-cargo-registry:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/target -w /src rust:1.99.1 \
    bash -c 'apt-get update -qq && apt-get install -y -qq libdbus-1-dev pkg-config \
             && cargo build --release --locked -p coderex-cli --bin coderex'
  ```

- The model endpoint is reached through a gateway on the host. Task containers
  get `http://host.docker.internal:8317`, which Docker Desktop resolves.
- `--coderex-binary` requires the path **on the host**; the adapter copies it
  into the container as `/tmp/coderex-local-binary`.

## Run history

### 2026-11-01 — `configure-git-webserver`, 7 runs, 0 resolved

The task asks for a git server answering `git clone user@server:/git/server`
with a post-receive hook deploying to an nginx on `:8080`. Its test clones with
**`git@localhost` on port 22, password `password`**, pushes, and fetches
`http://localhost:8080/hello.html`.

| Run | `failure_mode` | Test verdict | Root cause |
|---|---|---|---|
| `tb-20261101-052106` | unset | HTTP 000 | CodeRex ended the turn early (see defect 1) |
| `tb-20261101-054858` | unset | HTTP 000 | same |
| `tb-20261101-060424` | unset | HTTP 000 | same |
| `tb-20261101-061448` | unset | HTTP 000 | same — 89 s, 1 turn, no work beyond installing git |
| `tb-fixed-182822` | unknown_agent_error | HTTP 000 | streaming request hit the whole-request timeout (defect 2) |
| `tb-final-200330` | unset | HTTP 000 | ran the full 18 min; built a design the test does not accept |
| `tb-accept-202553` | test_timeout | HTTP 404 | nginx up, file not in the web root; grading timed out |

**Final verdict: model design substitution, not a CodeRex defect.** With the
harness defects fixed the agent runs to completion, but it builds its own
service rather than the one specified:

| Requirement | What the agent built |
|---|---|
| bare repo at `/git/server` | `/srv/git/repos` — `/git/server` never appears in the transcript |
| sshd on port 22 | its own sshd on 2222 |
| `git` user, password `password` | key-only auth; `chpasswd` never ran |
| web root `/var/www/html` | an fcgiwrap/`git daemon` design; `/var/www/html` never appears |

The final answer states the alternative design explicitly ("Repos live in
`/srv/git/repos`"), so this is a deliberate substitution, not an oversight.
The instruction is genuinely ambiguous — it says `user@server`, the test uses
`git@localhost` — but `/git/server` is written literally in the task, and the
agent replaced it.

### 2026-11-01 — `hello-world`, 3 runs, **1 resolved**

The simplest task in the dataset: write `/app/hello.txt` with exactly
`Hello, world!\n` and create nothing else. It was chosen to separate "CodeRex
can finish a task at all" from the design-substitution problem above.

| Run | `failure_mode` | Agent result | Root cause |
|---|---|---|---|
| `tb-hello-213746` | parse_error | never ran | the binary could not start — `GLIBC_2.39 not found` |
| `tb-hello-musl-222541` | test_timeout | **correct, 26 s** | the grading script's `apt-get update` exceeded the 60 s test budget |
| `tb-hello-graded-223630` | unset | **correct, 26 s** | **1/1 resolved** — both assertions passed |

**This is the first task CodeRex has passed on Terminal-Bench**, and it is a
clean result: `failure_mode: unset` and `parser_results` =
`{test_hello_file_exists: passed, test_hello_file_content: passed}`. The two
earlier failures were both environment, not agent.

#### Why the binary failed to start

The gnu build was linked against the toolchain image's newer glibc:

```
/usr/local/bin/coderex: /lib/aarch64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found
INSTALL_FAIL_STATUS
```

`hello-world` builds on `python-3-13` (Debian **bookworm**, glibc 2.36);
`configure-git-webserver` builds on `ubuntu-24-04` (glibc 2.39), which is why
the same binary worked there. Build a **static** binary instead — it works on
both, and on whatever base a later task uses:

```bash
docker run --rm --platform linux/arm64 \
  -v "$PWD":/src:ro -v /tmp/coderex-musl-target:/target \
  -v /tmp/coderex-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target -w /src rust:1.99.1 \
  bash -c 'apt-get update -qq && apt-get install -y -qq musl-tools libdbus-1-dev pkg-config \
           && rustup target add aarch64-unknown-linux-musl \
           && cargo build --release --locked --target aarch64-unknown-linux-musl \
                -p coderex-cli --bin coderex'
# /tmp/coderex-musl-target/aarch64-unknown-linux-musl/release/coderex
# -> "statically linked"; ~11 min cold
```

**Use the static build for Terminal-Bench.** A gnu build only runs on tasks
whose base image is at least as new as the toolchain image.

#### Why the grade timed out

`tests/setup-uv-pytest.sh` starts with `apt-get update` to install `curl`, then
fetches uv. Here the Debian package index (8.7 MB) downloads at 28–68 kB/s, so
the update alone takes 2–3 minutes — while the task's `max_test_timeout_sec`
is **60 s**. The log ends mid-download at 56 %:

```
56% [4 Packages 4678 kB/8689 kB 54%]   68.6 kB/s 1min 3s
```

The budget is the task's `max_test_timeout_sec`, overridden globally by the
harness flag `--global-test-timeout-sec` — exposed as `--test-timeout` on the
runner, which is what turned this run into a pass:

```bash
scripts/external-benchmarks/terminal-bench/run.sh \
  --task hello-world --test-timeout 900 ...
```

`configure-git-webserver` hit the same wall (`tb-accept-202553`: HTTP 404 was
recorded, then `test_timeout`). **Treat any `test_timeout` as an environment
result, not a task result, and re-grade with a larger budget before drawing a
conclusion.**

### 2026-11-02 — first 5-task batch, **4/5 resolved**

Five of the twelve `easy` tasks, chosen because they need no network at grading
time (the rest download datasets or scrape a live site). Run offline against
the cached dataset, `--n-concurrent 2`, 411 s total.

| Task | Result | Notes |
|---|---|---|
| `fix-permissions` | ✅ resolved | make `process_data.sh` runnable |
| `grid-pattern-transform` | ✅ resolved | 3/3 transformation cases |
| `processing-pipeline` | ✅ resolved | 9/9 checks, including re-running the pipeline |
| `csv-to-parquet` | ✅ resolved | 2/2 checks |
| `extract-safely` | ❌ unresolved | wrote the answer to the wrong file — see below |

All five had `failure_mode: unset`, so every one is a genuine agent result.

#### `extract-safely`: found the answer, saved it to the wrong place

> **Superseded.** This diagnosis was wrong: the prompt was truncated, so the
> model never saw the sentence naming `/app/solution.txt`. With the fix it
> resolves 3/3 — see *the stdin truncation* below. The paragraph is kept
> because it is the mistake this record is meant to prevent.

The task asks to extract one file out of a tar that also contains a
`sensitive/` directory (credentials, a 30 MB blob), and write its contents to
`/app/solution.txt`. The agent's approach was correct and careful — it listed
the archive first, then extracted **only the named member** into
`/app/extracted`, and verified nothing from `sensitive/` touched the disk.

It found the right text (`The answer is: extract_without_compromise`) but never
wrote `/app/solution.txt`; the content only appeared in the extracted tree and
in its own answer. Two of the three checks passed
(`test_system_logs_exists`, `test_no_sensitive_exposure`); `test_solution_found`
failed on `os.path.exists("/app/solution.txt")`.

This is the same class as `configure-git-webserver`: the model satisfies its
own idea of the task and drops a literal requirement ("write it to
`/app/solution.txt`"). It is not a CodeRex defect — the stream ended cleanly
with `end_turn`.

### 2026-11-02 — the stdin truncation, and what it invalidated

Every result above was measured with a CodeRex that **only read the first line
of a piped prompt**. `coderex -p` used `read_line`, so a multi-line task
description reached the model as its first line and nothing else. The model
then answered what it could see, and the turn still ended as `success`, so no
`failure_mode` revealed it.

It was caught by reading the model's own words in `openssl-selfsigned-cert`:

> No requirements list came through with the message (it ends at "the following
> requirements:")

That was literally true: the 1.5 KB numbered list after that phrase never
arrived. Fixed in `orchestrator.rs` (`read_to_string`), with
`a_multi_line_stdin_prompt_reaches_the_model_whole` as the regression test.

**Three tasks that had been scored as failures now pass, with no other
change** — same model, same instructions, same images:

| Task | Before | After |
|---|---|---|
| `heterogeneous-dates` | 0/3 assertions | **resolved** |
| `openssl-selfsigned-cert` | 0/6 assertions | **resolved** |
| `new-encrypt-command` | 0/1 | **resolved** |
| `extract-safely` | `test_solution_found: failed` | **resolved** (3/3) |

`extract-safely` had been written up here as "the model satisfies its own idea
of the task" — that was wrong. It never saw the sentence naming
`/app/solution.txt`. The same caveat applies to the earlier
`configure-git-webserver` write-up: its paths and port were in lines the model
never received.

### Current state after the fix

Seven tasks measured with a correct prompt path:

| Task | Result |
|---|---|
| `hello-world` | ✅ |
| `fix-permissions` | ✅ |
| `grid-pattern-transform` | ✅ |
| `processing-pipeline` | ✅ |
| `csv-to-parquet` | ✅ |
| `heterogeneous-dates` | ✅ (was failing) |
| `openssl-selfsigned-cert` | ✅ (was failing) |
| `new-encrypt-command` | ✅ (was failing) |
| `extract-safely` | ✅ (was failing) |
| `configure-git-webserver` | ❌ still failing |

`configure-git-webserver` now gets much further — `/git/server` appears in the
transcript (previously zero occurrences), nginx listens on 8080, and the web
check returns **HTTP 400** instead of 000. The blocker is authentication: the
grading test's `expect` script sends the password to a `git clone` process that
has already exited (`send: spawn id exp8 not open`), because the agent set up
key-only SSH and never ran `chpasswd`. That is a genuine behavioural
difference, not a plumbing failure.

**Lesson for this document:** before scoring a task, confirm the model
actually received the instruction. A prompt-delivery bug looks exactly like a
model that ignores requirements, and it produced three wrong conclusions here
before it was found.

### 2026-11-02 — `fix-git` and `sanitize-git-repo`

Both tasks clone a public GitHub repository while building their task image,
so they were the first to fail at `docker compose build`, before any agent ran.
The container can reach the host proxy at `host.docker.internal:1087`, and two
changes are needed for it to take effect: the compose file has to pass the
proxy as a build arg, **and** the Dockerfile has to declare it — an undeclared
build arg is not visible to `RUN`. With those in place the clone succeeds.

| Task | Build | Result |
|---|---|---|
| `fix-git` | still fails | the source repository no longer exists on GitHub |
| `sanitize-git-repo` | builds with the proxy | **2/3 pass** — over-scopes one extra file |

`fix-git`'s setup clones `TheMikeMerrill/personal-site`, which now returns
`Repository not found` from every path tried (direct, proxied). This is a stale
dataset dependency, not a network or proxy failure, and it cannot be fixed
from CodeRex's side.

`sanitize-git-repo` builds and starts. Its first attempt was refused by the
local gateway (`kiwanod: unknown API key`), so it was re-run against the direct
DeepSeek endpoint (`https://api.deepseek.com/anthropic`, model `deepseek-flash`,
key read from `.env`). That run completed normally.

Result: **2 of 3 assertions pass** — secret removal and replacement both
succeed. The failure is `test_no_other_files_changed`: the agent sanitised one
extra file outside the allowed set
(`exp_data/datasets/tokenized/rw_v2_fasttext_openhermes_vs_rw_v2_bigram_0.1_arcade100k.json`),
because its broader sweep found a credential there too. The grader only
permits changes to the contaminated paths, so the extra edit fails the run.
That is over-scoping by the model, not a CodeRex defect.

Recorded failure signature for this task: `test_removal_of_secret_information`
and `test_correct_replacement_of_secret_information` pass, while
`test_no_other_files_changed` raises
`ValueError: File exp_data/datasets/tokenized/rw_v2_fasttext_openhermes_vs_rw_v2_bigram_0.1_arcade100k.json has been changed`.

## CodeRex defects the benchmark exposed

All three were real, and all three are fixed on `main`:

1. **A truncated answer was reported as success.** `max_tokens` 4096 was
   exhausted by reasoning before any text or tool call; the loop ended the run
   and `repl.rs` reported every non-`MaxTurns` reason as
   `subtype: success, is_error: false`. Proven by instrumenting the loop:

   ```
   provider answer ended turn=9 stop_reason=max_tokens tool_calls=0 content_chars=0
   turn loop decided reason=token limit stop ... step=Break(TokenLimitStop)
   ```

2. **The whole-request timeout killed long streams.** One `reqwest::Client`
   with a 120 s `timeout` served every request, and `timeout` covers the
   response body; a turn that thought past two minutes died mid-stream.
   Streaming now uses a separate client with `read_timeout` (gap between
   chunks).

3. **Cassette maintenance was the only red thing on `main`.** Unrelated to the
   benchmark, but it blocked verifying any of this:
   `5196c02`/`2e9201b` changed the system prompt without refreshing the
   recorded digests, leaving `e2e_code_agent` and `e2e_equivalence` failing
   with `cassette_stale`.


### Known, unfixed

- **`UNKNOWN_CONTEXT_WINDOW` is still 32 000.** For a model CodeRex does not
  recognise, it compacts against a 32 K guess. The benchmark runs worked around
  this with `CODEREX_CONTEXT_WINDOW=131072`; the guess itself is unchanged, so
  an unknown model with a larger window still compacts far too early.
- **Compaction can burn its retry budget on a no-op.** Observed at the 32 K
  guess: a pass reported success while removing 35 tokens and *growing* the
  message list (39 → 44 → 47) before the turn hard-stopped with
  "context window is full".

## Running the batch offline

The harness resolves a dataset name through its **registry**, a GitHub raw URL:

```
terminal_bench/registry/client.py: DEFAULT_REGISTRY_URL =
  https://raw.githubusercontent.com/laude-institute/terminal-bench/main/registry.json
```

When that host is unreachable the run dies before a single task starts, with
`RemoteDisconnected` from inside the registry client — which looks like a
model-endpoint failure but is not. Point the runner at the cached dataset
instead:

```bash
scripts/external-benchmarks/terminal-bench/run.sh \
  --dataset-path ~/.cache/terminal-bench/terminal-bench-core/0.1.1 \
  --task <task-id> ...
```

`--dataset-path` and `--dataset` are mutually exclusive in the harness config,
so the runner passes exactly one.

## Reproducing a run

```bash
export ANTHROPIC_API_KEY=...            # or CODEREX_API_KEY
export ANTHROPIC_BASE_URL=http://host.docker.internal:8317
export ANTHROPIC_MODEL=...
export CODEREX_CONTEXT_WINDOW=131072    # only while the 32K guess is unfixed

scripts/external-benchmarks/terminal-bench/run.sh \
  --task <task-id> \
  --dataset-path ~/.cache/terminal-bench/terminal-bench-core/0.1.1 \
  --coderex-binary /tmp/coderex-musl-target/aarch64-unknown-linux-musl/release/coderex \
  --timeout 2700 --max-turns 200 --test-timeout 900 \
  --run-id <run-id>
```

`--test-timeout` is not optional in practice on this machine: the tasks'
own 60 s budgets are shorter than an `apt-get update` here.

`--no-rebuild` reuses task images between runs; `--n-concurrent N` runs several
tasks at once. Each task still needs its own container — the tasks conflict
(package installs, `/etc`, ports like 8080), and the grading tests attribute
results to a container's final state.
