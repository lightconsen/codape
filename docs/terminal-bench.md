# Terminal-Bench: what runs, and what it has found

Terminal-Bench is the external agent benchmark Codape is measured against. The
runner lives in [`scripts/external-benchmarks/terminal-bench/`](../scripts/external-benchmarks/terminal-bench/)
and is described there; this file is the **record of results**, and it is the
place to look before blaming a model for a result Codape caused, or vice
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
| `<task>/*/sessions/agent.log` | the last lines of Codape's stream-json transcript |
| `<task>/*/sessions/tests.log` | the grading test's real output, including the assertion text |
| `<task>/*/panes/post-agent.txt` | the terminal state when the agent stopped |
| `/logs/codape-stream.jsonl` (in-container) | the **full** transcript, copied there by the adapter |

`failure_mode` is the first thing to check, because it says whether the agent
ever got a fair test:

| `failure_mode` | Meaning |
|---|---|
| `unset` | the agent exited cleanly and the task's tests ran and scored it |
| `unknown_agent_error` | Codape's `result` frame reported an error (see the subtype) |
| `test_timeout` | the grading test itself timed out — the run was not scored on merit |
| `agent_timeout` | Codape exceeded its own timeout |
| `parse_error` | the harness found no pytest summary, so it never scored the run — usually the test script died during its own bootstrap (`apt-get update`, installing `uv`) |

`parse_error` and `test_timeout` are both "the test never produced a verdict".
Check `panes/post-test.txt`: if its last line is a download progress bar, the
test was cut off while installing its own dependencies, and the result says
nothing about the agent.

A trial with `failure_mode: unset` and `accuracy: 0` is a **model or prompt
result**. A trial with any other mode is a **plumbing result** and says nothing
about the model until the plumbing is fixed.

One shape the table does not cover is `unknown_agent_error` with
`parser_results: null` and every timestamp null. That row never reached the
test phase: `_run_trial` raised before it got there. On this host the cause has
been a full disk — the harness writes `post-agent.txt` without catching
`OSError`, so `[Errno 28] No space left on device` ends the trial and leaves a
row that reads like a crash. Search the run log for `No space left on device`
before blaming the agent.

## Environment on this machine

- macOS on arm64; Docker runs `linux/arm64` task images.
- There is no published aarch64 Codape release, so runs use
  `--codape-binary` with a locally cross-built **static musl** binary. A
  glibc build against `rust:1.99.1` links against glibc 2.39, which the
  `bookworm` task images (2.36) cannot load — `GLIBC_2.39 not found` before
  the agent starts. Build static instead:

  ```bash
  export http_proxy=http://127.0.0.1:1087 https_proxy=http://127.0.0.1:1087
  docker run --rm --platform linux/arm64 \
    -v "$PWD":/src:ro -v /tmp/codape-musl-target:/target \
    -v /tmp/codape-cargo-registry:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/target -w /src rust:1.99.1 \
    bash -c 'rustup target add aarch64-unknown-linux-musl \
             && apt-get update -qq && apt-get install -y -qq musl-tools pkg-config libdbus-1-dev \
             && cargo build --release --locked --target aarch64-unknown-linux-musl \
                  -p codape-cli --bin codape'
  ```

  The result is `/tmp/codape-musl-target/aarch64-unknown-linux-musl/release/codape`.
  Two things to get right: `rustup target add` must run **in the same
  container invocation** every time (the target is not in the image), and the
  first build downloads the whole registry — it takes 20+ minutes on this
  link, so run it in the background and check `stat` on the binary rather than
  assuming an earlier file is current.

- The model endpoint is reached through a gateway on the host. Task containers
  get `http://host.docker.internal:8317`, which Docker Desktop resolves.
- **Provider.** Batches 1–7 used DeepSeek. From the eighth batch on, runs use
  the KIMI provider from `~/.claude/settings.kimi.json`: `k3` at
  `https://api.kimi.com/coding`, context window 1 048 576. The key is read from
  that file at run time and exported into the runner's environment; it is never
  written to the dataset, the run directory, or a command line. The adapter's
  `PASSTHROUGH_ENV` forwards `ANTHROPIC_*` and `CODAPE_*` into the container.
  Give `CODAPE_CONTEXT_WINDOW` the real window — Codape assumes 32 000 for a
  model it does not recognise. A trailing slash on the base URL is harmless:
  `request_url` trims it before appending `/v1/messages`
  (`crates/core/src/api_client.rs:1002`).
- `--codape-binary` requires the path **on the host**; the adapter copies it
  into the container as `/tmp/codape-local-binary`.

### Linux x86_64 host (from 2026-11-04)

The batches from the twenty-sixth on ran on a second machine, and several of
the constraints above do not apply there:

- Linux 6.8 on x86_64, 16 vCPU, 61 GB RAM; plain Docker Engine, not Docker
  Desktop. Task images are `linux/amd64`.
- No cross build and no proxy are needed. The native target works, and the
  provider, `huggingface.co`, `github.com` and `dl-cdn.alpinelinux.org` are
  reachable directly, so image builds use the unpatched network path.
- The binary is `/tmp/codape-0.1.3`, the published 0.1.3 release rebuilt as
  `x86_64-unknown-linux-musl` (`static-pie linked`, stripped), run with
  `--codape-binary`.
- The model endpoint is `https://opencode.ai/inference/go/anthropic` with
  model `deepseek-v4.1-flash` and the header `x-opencode-session` supplied
  through `CODAPE_CUSTOM_HEADERS`; `CODAPE_CONTEXT_WINDOW=131072`. The key
  and base URL are read from `~/.claude/settings.json` at run time, never
  written into the dataset or a command line.
- The root filesystem is 50 GB. The docker build cache filled it during the
  first sweep (1.6 GB free, several trials died with `ENOSPC` while writing
  their pane files); `docker builder prune -af` recovered 13.4 GB. Every
  batch after that was preceded by a free-space check. It filled again on
  2026-11-04 from a trial's own writable layer — a 17 GB `/app/work` — which
  no prune can reclaim while the container runs; long trials need a mid-run
  `df`/`du` check as well.

## Run history

### 2026-11-01 — `configure-git-webserver`, 7 runs, 0 resolved

The task asks for a git server answering `git clone user@server:/git/server`
with a post-receive hook deploying to an nginx on `:8080`. Its test clones with
**`git@localhost` on port 22, password `password`**, pushes, and fetches
`http://localhost:8080/hello.html`.

| Run | `failure_mode` | Test verdict | Root cause |
|---|---|---|---|
| `tb-20261101-052106` | unset | HTTP 000 | Codape ended the turn early (see defect 1) |
| `tb-20261101-054858` | unset | HTTP 000 | same |
| `tb-20261101-060424` | unset | HTTP 000 | same |
| `tb-20261101-061448` | unset | HTTP 000 | same — 89 s, 1 turn, no work beyond installing git |
| `tb-fixed-182822` | unknown_agent_error | HTTP 000 | streaming request hit the whole-request timeout (defect 2) |
| `tb-final-200330` | unset | HTTP 000 | ran the full 18 min; built a design the test does not accept |
| `tb-accept-202553` | test_timeout | HTTP 404 | nginx up, file not in the web root; grading timed out |

**Final verdict: model design substitution, not a Codape defect.** With the
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
`Hello, world!\n` and create nothing else. It was chosen to separate "Codape
can finish a task at all" from the design-substitution problem above.

| Run | `failure_mode` | Agent result | Root cause |
|---|---|---|---|
| `tb-hello-213746` | parse_error | never ran | the binary could not start — `GLIBC_2.39 not found` |
| `tb-hello-musl-222541` | test_timeout | **correct, 26 s** | the grading script's `apt-get update` exceeded the 60 s test budget |
| `tb-hello-graded-223630` | unset | **correct, 26 s** | **1/1 resolved** — both assertions passed |

**This is the first task Codape has passed on Terminal-Bench**, and it is a
clean result: `failure_mode: unset` and `parser_results` =
`{test_hello_file_exists: passed, test_hello_file_content: passed}`. The two
earlier failures were both environment, not agent.

#### Why the binary failed to start

The gnu build was linked against the toolchain image's newer glibc:

```
/usr/local/bin/codape: /lib/aarch64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found
INSTALL_FAIL_STATUS
```

`hello-world` builds on `python-3-13` (Debian **bookworm**, glibc 2.36);
`configure-git-webserver` builds on `ubuntu-24-04` (glibc 2.39), which is why
the same binary worked there. Build a **static** binary instead — it works on
both, and on whatever base a later task uses:

```bash
docker run --rm --platform linux/arm64 \
  -v "$PWD":/src:ro -v /tmp/codape-musl-target:/target \
  -v /tmp/codape-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target -w /src rust:1.99.1 \
  bash -c 'apt-get update -qq && apt-get install -y -qq musl-tools libdbus-1-dev pkg-config \
           && rustup target add aarch64-unknown-linux-musl \
           && cargo build --release --locked --target aarch64-unknown-linux-musl \
                -p codape-cli --bin codape'
# /tmp/codape-musl-target/aarch64-unknown-linux-musl/release/codape
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
`/app/solution.txt`"). It is not a Codape defect — the stream ended cleanly
with `end_turn`.

### 2026-11-02 — the stdin truncation, and what it invalidated

Every result above was measured with a Codape that **only read the first line
of a piped prompt**. `codape -p` used `read_line`, so a multi-line task
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
from Codape's side.

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
That is over-scoping by the model, not a Codape defect.

Recorded failure signature for this task: `test_removal_of_secret_information`
and `test_correct_replacement_of_secret_information` pass, while
`test_no_other_files_changed` raises
`ValueError: File exp_data/datasets/tokenized/rw_v2_fasttext_openhermes_vs_rw_v2_bigram_0.1_arcade100k.json has been changed`.

### 2026-11-02 — third batch, **3/5 resolved**

Five more self-contained tasks, this time with the direct DeepSeek endpoint
and `deepseek-flash`. Two of the three failures are dataset inconsistencies,
not agent errors.

| Task | Result | Notes |
|---|---|---|
| `modernize-fortran-build` | ✅ resolved | 3/3 checks |
| `git-workflow-hack` | ✅ resolved | `test_token_leak` |
| `sqlite-db-truncate` | ✅ resolved | `test_json_data` |
| `polyglot-c-py` | ❌ unresolved | instruction and test use different file names |
| `nginx-request-logging` | ❌ unresolved | instruction and test put settings in different files |

**`polyglot-c-py` is internally inconsistent.** The instruction says to write
`/app/main.c.py`; `test_fibonacci_polyglot` runs `python3 /app/main.py.c` and
`gcc /app/main.py.c`. An agent that follows the instruction writes
`main.c.py`, and the test then cannot find the file. The task cannot be scored
as specified without also creating the other name.

**`nginx-request-logging` has the same class of contradiction.** The
instruction explicitly says *"Place the configuration in
`/etc/nginx/conf.d/benchmark-site.conf`"*, and the agent did exactly that.
`test_nginx_config_settings` then looks for the log-format fields and the
`limit_req_zone` rate limit in **`/etc/nginx/nginx.conf`**, a different file.
Port/root checks read the conf.d file, but the log-format and rate-limit
checks read the main config. A correct conf.d-only setup therefore fails even
though the site serves, logs, and rate-limits correctly (the other seven
assertions pass).

These two are recorded as **dataset problems**, not model or Codape problems.
The remaining tasks pass end-to-end.

### 2026-11-02 — fourth batch, **3/5 resolved**

| Task | Result | Notes |
|---|---|---|
| `fibonacci-server` | ✅ resolved | 6/6 — server on :3000, F(0)/F(100), 400 handling |
| `sqlite-with-gcov` | ✅ resolved | 3/3 — compiled, on PATH, gcov enabled |
| `chess-best-move` | ✅ resolved | correct move from a board image |
| `vim-terminal-task` | ❌ unresolved | 4/5 pass; the script lacks the executable bit |
| `decommissioning-service-with-sensitive-data` | ❌ unresolved | 5/6 pass; archive layout mismatch |

**`vim-terminal-task`**: `test_python_script_executable` requires
`os.access("/app/text_processor.py", os.X_OK)`. The instruction never asks for
an executable file ("Using vim, create a Python script named
'text_processor.py' that: 1. Takes a filename as a command line argument …"),
and the script is invoked as `python /app/text_processor.py` by the other
tests — so it runs correctly and every functional check passes. The
requirement exists only in the test.

**`decommissioning-service-with-sensitive-data`**: five of six checks pass,
including shredding the originals, removing the config and PID file, and
producing the GPG archive. The failing assertion decrypts the archive and then
looks for `opt/sensitive_service_data/user_secrets.txt` — the archive is
expected to carry the full path. The agent archived the directory's *contents*.
Whether `tar` records the parent path depends on how the command is written
(`-C /opt sensitive_service_data` vs `-C /`), and the instruction does not say
which. The archive itself is valid and encrypted with AES256 as required.

Both failures are **underspecified requirements**, not Codape defects — and
unlike the earlier batches, the model's work is correct in every functional
respect that the instruction states.

Exact signatures, for matching a future run:

```
vim-terminal-task
  FAILED test_python_script_executable
         AssertionError: text_processor.py is not executable
  passed: test_python_script_exists, test_sample_text_exists,
          test_sample_text_content, test_script_output

decommissioning-service-with-sensitive-data
  FAILED test_archive_is_correctly_encrypted_and_contains_data
         AssertionError: Sensitive user_secrets.txt not found in decrypted archive.
         where exists = PosixPath(
           '/app/decrypted_test_output/extracted_files/opt/sensitive_service_data/user_secrets.txt')
  passed: test_original_sensitive_data_is_gone, test_config_file_is_gone,
          test_pid_file_is_gone, test_encrypted_archive_exists,
          test_intermediate_unencrypted_archive_is_gone
```

### 2026-11-02 — fifth batch, **2/5 resolved**

| Task | Result | Notes |
|---|---|---|
| `prove-plus-comm` | ✅ resolved | scored by its tests |
| `password-recovery` | ✅ resolved | scored by its tests |
| `jupyter-notebook-server` | ❌ unresolved | **Codape defect** — `error_loop_detected`, now fixed |
| `raman-fitting.easy` | ❌ unresolved | **dataset contradiction** — instruction says `amplitude`, test reads `["A"]` |
| `tmux-advanced-workflow` | ⚪ not scored | `parse_error` — the test died in its own `apt-get update` |

**`jupyter-notebook-server` — the loop detector false-positive.** The run ended
with `subtype: error_loop_detected` while the agent was legitimately waiting for
a long `pip install`. Six consecutive polls of the same command —

```
sleep 55; tail -3 /tmp/pip_install.log
sleep 55; tail -4 /tmp/pip_install.log
```

— each returned a *different* line (`Downloading httpcore-1.0.9…`,
`Downloading anyio-4.16.0…`, `10.2/10.2 MB 113.2 kB/s`). The detector already
knows that a call whose result changes is progress
(`from_tool_call_and_result`), but the engine never handed it the results: see
[the fixed defect](#the-loop-detector-never-saw-a-tool-result).

**`raman-fitting.easy` — the instruction contradicts the test.** The
instruction's JSON skeleton names the peak height `"amplitude"`:

```
"G": { "x0": …, "gamma": …, "amplitude": …, "offset": … }
```

The test reads `G_peak["A"]`, and so does the dataset's own reference
`solution.sh` (`json.dump({... "A": A_g ...})`). A model that follows the
instruction cannot pass. Here the agent did exactly that and wrote
`"amplitude": 8333.5`, so the failure is a plain `KeyError: 'A'` before any
value is compared:

```
FAILED ../tests/test_outputs.py::test_G_Peak - KeyError: 'A'
FAILED ../tests/test_outputs.py::test_2D_Peak - KeyError: 'A'
```

The fitted numbers themselves are defensible — `amplitude 8333.5 / 12594.3`
against expected `8382.69 / 12314.42` (G within 0.6 %, 2D within 2.3 %) — so
the failure is entirely the key name. A model that follows the instruction
cannot pass, and one that reads `solution.sh` rather than the instruction
would.

**`tmux-advanced-workflow` — never graded.** The harness found no pytest
summary (`Error parsing results … No short test summary info found`). The
grading script begins with `apt-get update`, which in `post-test.txt` is still
printing `61% [4 Packages 5294 kB/8689 kB]` when the pane was captured; at the
~120 kB/s this network gives the task container, the Debian index alone takes
several minutes. This is the same starvation that hit `hello-world` and is why
`--test-timeout 900` exists — but this trial was not launched with it. **That
this batch got `parse_error` rather than `test_timeout` is a harness difference,
not a Codape result.** Re-run with `--test-timeout 900` to score it.

There is a second, independent problem in this trial: the agent began with
`tmux kill-server 2>/dev/null`, which destroys **every** tmux session on the
machine — including the harness's own, which is how Terminal-Bench sends
keystrokes and captures panes. `panes/post-agent.txt` is the whole failure:

```
no server running on /tmp/tmux-0/default
```

and `codape-exit-code` is `137` (SIGKILL) rather than a clean exit. The
harness created the `agent` session and, at 05:12:00, the `tests` session; the
agent's `kill-server` took them both out and `tmux ls` afterwards showed only a
`tests` session that had been re-created. This is **model behaviour conflicting
with the harness's transport**, not a Codape bug — Codape ran the command the
model asked for — but it is worth knowing when scoring tmux tasks: a run that
kills the server cannot be graded no matter how good the work was.

The work that did happen before the truncation looked correct: session
`workflow` created, top pane split off, bottom pane split into two
(`split-v=0`, `split-h=0`, geometry `100×24` + `99×24`). The agent was starting
the monitor script when the 360 s task budget ran out.

Exact signature for the loop failure:

```
result
  subtype: error_loop_detected
  exit code 0 (the harness treats the subtype as the verdict)
```

### 2026-11-02 — sixth batch, **3/6 resolved**

Run with the loop-detector fix in the binary. `--n-concurrent 2`,
`--test-timeout 900`.

| Task | Result | Notes |
|---|---|---|
| `sqlite-db-truncate` | ✅ resolved | recovered all 8 rows from the truncated database file |
| `organization-json-generator` | ✅ resolved | 4/4 — schema, relationships and statistics all check out |
| `simple-web-scraper` | ✅ resolved | 5/5 — scrape, CSV, and report |
| `jupyter-notebook-server` | ❌ unresolved | **loop defect gone**; 2/4 — config file uses `ServerApp` only |
| `cron-broken-network` | ❌ unresolved | 1/2 — the test's expected body for `example.com` is from 2017 |
| `incompatible-python-fasttext` | ⚪ not scored | `docker compose build` fails: `huggingface.co` unreachable |

**`jupyter-notebook-server` — the fix holds.** This is the task the loop
detector killed in batch 5. Re-run, the agent polled the install **eleven**
times (`sleep 25…60; tail -5 /tmp/pip_install.log`), got a different line every
time, and ran to completion — `failure_mode: unset`, no `error_loop_detected`.
The remaining two failures are unrelated and both are the same modelling
choice: the agent wrote a `ServerApp`-based config (correct for
jupyter-server ≥ 2.0, which is what it installed) and kept
`c.NotebookApp.password` only for compatibility, but the test greps the file
for the exact strings `c.NotebookApp.password` / `c.NotebookApp.ip` /
`c.NotebookApp.certfile` and then parses the hash with
`re.search(r"c.NotebookApp.password = '(.*?)'")` — single quotes, `NotebookApp`
spelling. The config it produced is:
`c.ServerApp.password = "argon2:…"` (double-quoted) plus
`c.NotebookApp.password = "argon2:…"`. So `test_jupyter_config_exists` fails on
an exact substring, and `test_password_authentication` on the quote style. The
server does work — `test_jupyter_server_running` passes over HTTPS with the
password set. This is a **test that pins a deprecated config spelling**, and
the instruction does not say which spelling to use.

**`cron-broken-network` — the expected page no longer exists.** The agent was
asked to fix why `curl example.com` fails; the container's `/usr/bin/curl` had
been replaced by a no-op script on a cron schedule (the task's planted bug).
The agent found and fixed it — `test_curl_file_exists` passes, and the captured
`curl` output in the test shows a real 577-byte 200 OK. The second test then
compares the body against a 46-line HTML document hard-coded in
`test_outputs.py`, expecting >90 % of its lines to appear. The live page is now
minified onto one line:

```
<!doctype html><html lang=en><head><meta charset=utf-8><link rel=icon href=data:,>…
```

so only 8 of 46 expected lines match (`assert (8 / 46) > 0.9`). The dataset
predates the 2023+ example.com redesign. The agent's work was correct; the
fixture is stale. Note the test also `time.sleep(60)` before re-running curl,
so this task needs a test budget of at least ~90 s.

**`incompatible-python-fasttext` — never built.** Its Dockerfile downloads a
model from `huggingface.co`, which times out from this network even with the
proxy (`Connection timed out` on 199.59.148.247, then
`Network is unreachable` on the IPv6 address). No container, no agent, no
result. Fixing this needs the model fetched out-of-band and injected, which the
dataset does not support.

### 2026-11-02 — seventh batch, **3/5 resolved**

Run with the loop-detector fix. `--n-concurrent 2`, `--test-timeout 900`.

| Task | Result | Notes |
|---|---|---|
| `write-compressor` | ✅ resolved | 3/3 — built a compliant `data.comp` under the 2500-byte cap |
| `modernize-fortran-build` | ✅ resolved | 3/3 — Makefile rewritten, builds and runs |
| `fix-pandas-version` | ✅ resolved | 3/3 — pinned the pandas the code needs |
| `path-tracing-reverse` | ❌ unresolved | never wrote `mystery.c`; spent the whole budget decompiling |
| `git-multibranch` | ❌ unresolved | the test's `git checkout -b main` collides with the branch the clone lands on |

**`path-tracing-reverse` — ran out of budget solving the hard part.** The agent
did serious reverse engineering: it recovered the ray-tracing algorithm from
the binary, found the constants were single-precision (`1.3333333f` vs the
binary's `0x3FAAAAAB`), and was validating a Python model against the reference
image when the clock ran out — `agent_ended_at` is exactly `timeout + 180`
after `agent_started_at`. It never produced `/app/mystery.c`, so all three
tests fail on a missing file.

**The timeout was ours, not the task's.** `path-tracing-reverse` declares
`max_agent_timeout_sec: 360`, but the harness gives `--global-agent-timeout-sec`
precedence over the per-task value
(`harness.py:639`), and the runner passes `--timeout + 180`. Every task in
every batch therefore runs for the flag's duration, not its own. That is
deliberate for slow tasks and `apt-get`, but it turned a 6-minute task into a
48-minute one. When a task's intended budget matters, pass `--timeout` per
run and do not mix tasks of very different sizes.

**`git-multibranch` — the test's own script assumes a repository state it does
not create.** The server is not what fails. `verify.sh` clones
`git@localhost:/git/project` and then runs `git checkout -b main` in the clone.
That command only succeeds when the clone lands on some branch *other* than
`main`; several states fail it:

```
fatal: a branch named 'main' already exists   # clone is already on main with a commit
fatal: You are on a branch yet to be born     # .git/refs/heads/main is empty/garbage
fatal: 'refs/remotes/origin/main' has neither a valid OID nor a target  # empty ref on the server
```

The **batch-8** run is fully explained, and it is the model's choice.
The agent left `main` and `dev` both pushed and deployed. The clone therefore
lands on a real `main`, `checkout -b main` fails with *already exists*, and the
page the test finally reads is the agent's own test page, `<h1>MAIN branch
v2</h1>`, not `main branch content`. The agent had built a working server —
password SSH, both branches deploying in ~140 ms, a `post-receive` hook using
`git archive | tar -x` — and verified it end-to-end; it just left the repository
in a state its own test cannot pass.

**The batch-7 run is a different failure and is not fully explained here.**
`checkout -b main` reported *"on a branch yet to be born"*, and the clone had
printed a capitalised `(git@localhost) Password:` prompt that the script's
case-sensitive `expect "password:"` does not match. What is verified:

- It is **not** a git-version regression. On git 2.53.0 (the image's version,
  `FROM ubuntu:latest` → 26.04) `checkout -b main` succeeds on an unborn branch
  in every state a plain `git init` / `git clone` produces.
- The message needs an existing `.git/refs/heads/main` that is zero-length or
  unparseable. A zero-length ref on the *server* is worse — the clone dies with
  *"refs/remotes/origin/main has neither a valid OID nor a target"* — so whatever
  produced it was local to the clone.
- The agent's own transcript says it ran its verifier against a pristine
  repository and cleaned up test pushes, which makes the state hard to explain
  by the agent's actions alone.

The likely cause is the password prompt going unanswered: the test never sends
the password, and a clone aborted at the prompt is a plausible way to be left
mid-initialisation. But that chain is an inference, not a reproduction — I could
not get git to leave exactly this state from a failed clone. The spec's
under-specification is verified regardless: the instruction never says whether
the server starts with an empty repository or with populated branches, the test
requires exactly one of those, and it computes that from a branch the agent was
never told to leave empty. Batch 7 is recorded as a dataset problem; batch 8 as
both a dataset problem and the model's choice.

The `send: spawn id exp8 not open` that follows is the same second-password
failure seen in `configure-git-webserver`: once the first `expect` block ends
early, the next `send` has no live spawn.

### 2026-11-02 — eighth batch, first on KIMI, **1/2 scored**

The remaining two unscored tasks, re-run on the KIMI provider (`k3`,
`https://api.kimi.com/coding`, 1 M context) instead of DeepSeek.

| Task | Result | Notes |
|---|---|---|
| `incompatible-python-fasttext` | ✅ resolved | 3/3 — the fixture patch is the only thing that was missing |
| `git-multibranch` | ❌ unresolved | `test_multi_branch_https_deploy`; the clone arrives already on `main` — see the batch-7 note above |

**`incompatible-python-fasttext` — the fixture patch was the whole blocker.**
With the proxy build args in place the image builds, the model downloads, and
the agent diagnosed the numpy incompatibility. All three tests pass:
`test_predict_raises_no_error`, `test_correct_output`,
`test_site_packages_not_overly_changed` — the last one matters, because it means
the agent removed the offending dependency without gutting the rest of
`site-packages`. 322 s.

This is the **second** time it scored: `batch7b` (the DeepSeek re-run started
before the switch to KIMI) also passed 3/3, in 288 s, on a binary the agent had
already fixed. That run is recorded by the harness as `failure_mode:
unknown_agent_error`, but the agent was not at fault — the *provider* died
mid-run with a 401:

```
API error (401): Authentication Fails, Your api key: ****7b81 is invalid
```

The agent's last turn had already fixed the package, `codape-exit-code` is
`0`, and the test passed. This is a provider-key failure, not a Codape failure,
and it is the reason the switch to KIMI was made. Worth knowing when reading
`unknown_agent_error` in these results: check `codape-exit-code` and the last
`result` line before blaming the agent.

**`git-multibranch` — a different failure from batch 7, see above.** The agent
built a working server — password SSH, both branches deploying in ~140 ms, the
`post-receive` hook using `git archive | tar -x` rather than a work tree — and
verified it with its own end-to-end script, whose output is in its final
message (`<h1>MAIN branch v2</h1>`, `<h1>DEV branch deployed</h1>`). It then
left that state in the repository. The test clones, lands on `main`, and
`checkout -b main` fails with `a branch named 'main' already exists`; the page
it finally reads is the agent's own test page, not `main branch content`.

### 2026-11-02 — ninth batch, **3/5 resolved**

Five tasks that had never been run, all on KIMI (`k3`).

| Task | Result | Notes |
|---|---|---|
| `blind-maze-explorer-5x5` | ✅ resolved | 2/2 — explored the maze and wrote a map the test accepts |
| `crack-7z-hash.easy` | ✅ resolved | 2/2 — recovered the 4-digit password with john |
| `simple-sheets-put` | ✅ resolved | 3/3 — spreadsheet, sheet and cells through the HTTP API |
| `intrusion-detection` | ❌ unresolved | 5/7 — one dataset contradiction, one missing input check |
| `polyglot-rust-c` | ❌ unresolved | timed out at 900 s; right idea, never wrote `/app/main.c.rs` |

**`intrusion-detection` — the test greps for a string the instruction does not
require.** `test_intrusion_detector_content` asserts the *source text* contains
`logs/auth\.log`:

```python
assert re.search(r"logs/auth\.log", content), "Script should process auth.log"
```

The agent wrote `LOGS_DIR=/app/logs` and used `"$LOGS_DIR/auth.log"`, which is
the natural reading of "parse both auth.log and http.log files located in
/app/logs" — and its `test_intrusion_detector_execution` **passes**, so the
script really does read both logs and produce valid JSON. The test is checking
spelling rather than behaviour.

The second failure is the model's. `test_response_script_error_handling` feeds
`999.999.999.999` and expects an error or non-zero exit; the script printed

```
Block rule generated: iptables -A INPUT -s 999.999.999.999 -j DROP
```

and wrote an incident report for it. Nothing in the instruction asks for
validation, but a security response script that will emit a firewall rule for an
impossible address is a real robustness defect, not a test artifact.

**`polyglot-rust-c` — the model spent the whole budget on the hard part and
never delivered.** It identified the correct mechanism: an `ld` link script that
makes the same file valid Rust and valid C, so both `rustc` and `gcc` accept it.
It got gcc building and computing the right answer (`run=42`). But every
experiment ran under `cd /tmp` and it never wrote `/app/main.c.rs`, so the test
reads a missing file and fails. Exit code 124 — the harness's 900 s timer. The
stream also stopped on a tool result with no closing `result` frame, which is
the defect fixed as #7 below.

*The cwd was not stuck.* This was first written up here as a suspected Codape
defect, on the evidence that every Bash call began with `cd /tmp`. The stream
disproves it: the first `pwd`/`ls -la /app` runs in `/app`, and the `CurrentDir`
tool returns `/app`. `cd /tmp` was the model's own choice, and
`bash_tool.rs`'s child gets `ctx.cwd` on every call (`sandbox_shell.rs:66`) —
there is no persistent shell whose directory could drift. Recorded so the
suspicion is not raised twice.

Two Codape problems did fall out of this run: the single hard-coded WebSearch
backend and the missing closing event when a timeout ends a run. Both are now
fixed — see defects 5 and 7 in [Codape defects the benchmark
exposed](#coderx-defects-the-benchmark-exposed).

### 2026-11-02 — tenth batch, **2/5 resolved**

Five tasks that had never been run, all on KIMI (`k3`).

| Task | Result | Notes |
|---|---|---|
| `crack-7z-hash` | ✅ resolved | recovered the hash; the test passes |
| `create-bucket` | ✅ resolved | 2/2 — bucket exists and is public |
| `count-dataset-tokens` | ❌ unresolved | 900 s timeout; the container cannot reach huggingface.co |
| `raman-fitting` | ❌ unresolved | 900 s timeout; numpy never installed, no `results.json` |
| `qemu-alpine-ssh` | ⚠️ not measured | image build fails — upstream 404, before the agent runs |

**`qemu-alpine-ssh` — the task does not build on any machine today.** Its
Dockerfile starts from `debian:bullseye-slim` and installs `qemu-system-x86`,
which resolves to `qemu 5.2+dfsg-11+deb11u5`. Those files are gone from
Debian's security pool:

```
E: Failed to fetch http://deb.debian.org/debian-security/pool/updates/main/q/qemu/qemu-utils_5.2+dfsg-11+deb11u5_arm64.deb  404  Not Found
```

The pool now carries only 7.2 (bookworm) and 10.0 (trixie). This fails in
`docker compose build`, before the agent container exists, so it is a dataset
bit-rot failure and not a Codape result. It is listed as not measured rather
than as a failure — the harness itself records `unknown_agent_error`, but no
agent ever ran.

**`count-dataset-tokens` — the container has no route to huggingface.co.** The
task needs a dataset size from huggingface; the container reports:

```
huggingface.co FAIL [Errno 101] Network is unreachable
datasets-server.huggingface.co FAIL [Errno 101] Network is unreachable
```

while `pypi.org`, `files.pythonhosted.org`, `github.com` and `hf-mirror.com`
all answer. The agent routed around it well — it found the README through
`hf-mirror.com` and confirmed a 2.88 MB/s download from there — but the
per-file parquet listing it needed was on the blocked host, and the run hit the
900 s timer with `/app/answer.txt` never written. The host does reach
huggingface.co through the proxy (`/api/datasets/...` returns 200 in 2.2 s), so
this is the container's egress policy, not the task or the agent.

**`raman-fitting` — pip could not finish inside the tool timeout.** The agent
needed numpy and scipy. Every foreground `pip install` was killed:

```
tool 'Bash' timed out after 60s
```

so it moved pip to the background — the right move — and the background job was
still downloading when the 900 s timer fired. Two things are worth separating:
numpy is a 15.7 MB wheel that should not take minutes on a working link, and the
agent burned about 150 s on foreground retries before backgrounding. Its final
message even says "the regions look flat" over data that had loaded fine, so it
was reasoning against an incomplete picture. No `results.json` was written and
all three tests fail on the missing file.

### 2026-11-02 — eleventh batch, **2/5 resolved**

Five tasks that had never been run, all on KIMI (`k3`).

| Task | Result | Notes |
|---|---|---|
| `blind-maze-explorer-algorithm` | ✅ resolved | explored the maze and produced a map the test accepts |
| `path-tracing` | ✅ resolved (4/4) | reconstructed the image at 0.996 similarity; recorded as a failure only because of a Codape shutdown defect, now fixed |
| `conda-env-conflict-resolution` | ❌ unresolved | 900 s timeout; `environment.yml` was fixed correctly, conda never finished |
| `pytorch-model-cli` | ⚠️ not measured | test phase starved by `apt-get` at 49 kB/s; pytest never printed a summary |
| `sanitize-git-repo.hard` | ⚠️ not measured | image build: `git clone dclm` died mid-transfer |

**`path-tracing` — a run that passed every test was recorded as a failure, and
the cause was Codape.** The four assertions all pass:

```
PASSED test_image_c_exists  test_image_compiles
PASSED test_runs_and_produces_output  test_image_similarity
```

The result frame says `subtype: success`, and the image it rendered scores
0.9959 against the target. Yet the trial is still `unknown_agent_error`,
because `codape-exit-code` is `124`: the 900 s timer fired. The timeline
explains it — the `result` frame was written at 892.9 s, and the harness killed
the process at 900 s, seven seconds later.

Those seven seconds were not shutdown. With `CODAPE_LOG` on, the process
issues **one more non-streaming, tool-less model request after the result
frame** and waits 7.5 s for it:

```
15:15:53.064 provider request started  streaming=false tool_count=0
15:16:00.536 provider response received status=200
```

That request is session-title generation (`repl.rs:8160`), which runs
synchronously at the end of a headless turn. It is also pointless here: the
harness runs with `--no-session-persistence`, and `persist_headless_session`
returns early in that mode, so nothing can ever read the title back. The
request buys latency and throws away the answer — and if a turn ends within a
few seconds of its deadline, it costs the run its result.

The fix skips the title query when `no_persistence` is set; a regression test
counts side queries and fails against the old code. Measured on the same
machine, the gap between the result frame and process exit went from ~5.9 s to
~0.03 s. With persistence enabled the title is still generated, which a second
test pins down.

**`conda-env-conflict-resolution` — a correct fix, not enough time.** The
`environment.yml` came from a pytorch build that requires a `__cuda` virtual
package, and conda could not solve it even on a dry run. The agent read that
correctly and rewrote the file; `test_environment_yml_modified` passes. What it
could not do is install: with a slow repodata fetch, `conda env create` never
finished before the timer, so `datasci` does not exist and the two dependent
tests fail. This is a throughput failure, not a reasoning one.

**`pytorch-model-cli` — apt at 49 kB/s.** Its test phase installs a large
dependency set, and the recorded rate was `49.3 kB/s` with an ETA of 31
minutes; the harness gave up before pytest printed a summary, which is why the
harness reports `parse_error` rather than a test failure. The agent had also
spent its budget waiting on `apt`/`pip` installs. Not measurable on this link.

**`sanitize-git-repo.hard` — the image build loses the clone.** `setup.sh` runs
`git clone` of the `dclm` dataset during `docker build`:

```
error: RPC failed; curl 56 GnuTLS recv error (-9): Error decoding the received TLS packet.
fatal: early EOF
```

The clone is 12+ minutes in and then drops, so the image never exists and no
agent runs. As with `qemu-alpine-ssh` in the tenth batch, this is recorded as
not measured rather than as a Codape failure.

### 2026-11-02 — twelfth batch, **4/5 resolved**

Five tasks that had never been run, all on KIMI (`k3`), using the binary built
with the title-query fix from the eleventh batch.

| Task | Result | Notes |
|---|---|---|
| `blind-maze-explorer-algorithm.easy` | ✅ resolved (11/11) | 241 s |
| `blind-maze-explorer-algorithm.hard` | ✅ resolved (11/11) | 288 s |
| `swe-bench-langcodes` | ✅ resolved | 184 s; the whole test is one patched assertion |
| `swe-bench-fsspec` | ✅ resolved (135/135) | 474 s; the largest test count any task here has produced |
| `play-zork` | ❌ unresolved | 2700 s, the full budget; 2/2 tests fail on a missing `/app/answer.txt` |

The four passes are unremarkable in the best way — no plumbing failures, no
`parse_error`, and `failure_mode: unset` throughout. Two `swe-bench` tasks
passing matters for a different reason: they need a real repository checkout,
a patch applied to it, and a test suite run under a freshly created venv, all
of which is ordinary work for the agent and all of which worked.

**`play-zork` is a genuine model result, and it also re-exposed the timeout
defect from the ninth batch.** The task asks for Zork finished at the maximum
score of 350, with the ending message in `/app/answer.txt`. The agent played
the game — it reached 54 points and repeatedly issued `@move_obj` commands
through the frotz debugger — but never wrote the file, so both assertions fail
on `FileNotFoundError`.

Two things are worth separating here:

- **The budget question is a model question.** 2700 s is the full agent
  timeout. Whether Zork at 350 is reachable in that window is not something
  this run can answer; the run spent it exploring, not finishing.
- **The defect is Codape's.** The stream ends on a `user` (tool-result) frame
  and `codape-exit-code` is 124 — exactly the shape recorded for
  `polyglot-rust-c` in the ninth batch. The last tool call before the timer
  is also instructive:

  ```
  [SECURITY WARNING: Dangerous: > /tmp/mapoutside.out (overwrites file)]

  [Exit code: -1]
  ```

  A redirect to a scratch file was judged dangerous and the command never ran.
  Worth knowing: the warning is informational for stdout commands and
  effectively a block here.

The timeout defect is now fixed — see defect 7 below. A `--print` turn with a
stream writes `subtype: "interrupted"` and exits 143 when a signal ends it,
so a run stopped from outside is no longer indistinguishable from a crash.

### 2026-11-02 — thirteenth batch, **4/5 resolved**

Five more never-run tasks on KIMI (`k3`), using the binary built with the
signal fix from the twelfth batch — this is the first batch that could
exercise it. All five images were pre-built before the run to separate
"cannot build here" from "the agent failed"; `qemu-startup` was dropped for
that reason (see below).

| Task | Result | Notes |
|---|---|---|
| `swe-bench-astropy-1` | ✅ resolved | the nested-`CompoundModel` separability bug |
| `swe-bench-astropy-2` | ✅ resolved | the `ascii.qdp` upper-case QDP bug |
| `crack-7z-hash.hard` | ✅ resolved | `john` built from source in the image; answer `honeybear` |
| `pytorch-model-cli.easy` | ✅ resolved | C CLI against a bundled `model.pth` |
| `gpt2-codegolf` | ❌ unresolved | 2700 s, the full budget |

Four passes, `failure_mode: unset` throughout. The two `swe-bench` tasks are
the first astropy ones tried; like the fsspec and langcodes tasks they need a
real checkout, a patch and a test suite, and both worked.

**`gpt2-codegolf` is a model result, and the timeout classification is now
right.** The agent wrote `/app/gpt2.c`, then iterated on it for the full
window — 84 tool calls, almost all `Edit` to the same file — and the test
fails on the *first* assertion, before any sampling is compared:

```
E       AssertionError: File /app/gpt2.c is larger than 5000 bytes
E       assert 5030 < 5000
```

It came within 30 bytes of the size limit and ran out of time before making
them. The run ends exactly as `play-zork` did — except that this time the
stream has its closing frame:

```
{"type":"result","subtype":"interrupted","is_error":true,
 "duration_ms":2700022,"num_turns":1,
 "error":"SIGTERM ended the run before the turn finished"}
```

and the harness reports `agent_timeout`, not `unknown_agent_error`. That is
the whole point of defect 7: the same run, recorded for what happened to it
rather than for what it looked like.

**`qemu-startup` is unrunnable here, like `qemu-alpine-ssh`.** Its image
installs `qemu-system-x86` from `debian:bullseye-slim`, and the bullseye
security pool has deleted the exact versions apt resolves to, so the build
dies before the agent runs:

```
E: Failed to fetch http://deb.debian.org/debian-security/pool/updates/main/... 404  Not Found
```

That is the same failure recorded for `qemu-alpine-ssh` in the seventh batch,
now confirmed on a second task from the same base image. It is a dataset
problem, not a Codape one.

### 2026-11-02 — fourteenth batch, **2/3 resolved, and one pass after a timeout**

Three more never-run tasks on KIMI (`k3`).

| Task | Result | Notes |
|---|---|---|
| `pytorch-model-cli.hard` | ✅ resolved (6/6) | C CLI + `weights.json` + `prediction.txt` |
| `cartpole-rl-training` | ✅ resolved (5/5) | **after the agent timeout fired**; see below |
| `run-pdp11-code` | ❌ unresolved | the agent never wrote `/app/out.txt` |

**`cartpole-rl-training` is the case the timeout fix was for.** The trial is
recorded as `agent_timeout` — and it resolved anyway. The agent trained the
model, saved it, and was still polling its own training log when the timer
fired at 2700 s; the stream closes with

```
{"type":"result","subtype":"interrupted","is_error":true,
 "duration_ms":2700019,"num_turns":1,
 "error":"SIGTERM ended the run before the turn finished"}
```

All five tests then pass against the container as the agent left it. Nothing
in the harness treats `agent_timeout` as fatal — `harness.py:752` continues
into the test phase precisely so a partial answer can still be scored — so
the run resolved. Before the twelfth batch's fix that same run would have
been `unknown_agent_error`, which reads as "the agent crashed" rather than
"the agent ran out of clock with the work already done".

**`run-pdp11-code` is a genuine wrong answer, not a plumbing failure.** The
agent identified the file correctly — a PDP-11 Unix `a.out` (octal dump,
magic `000407`) — built `apout` from source and ran it. Its output is real
and deterministic, and it is wrong by a constant:

```
expected: 17 -60 -30 -15 -58 -29 -15 -8 -54 -27
          -64 -32 -16 -8 -4 -2 -1 -1 -1 -1
reported: 17 36 66 81 38 67 81 88 42 69
          32 64 80 88 92 94 95 95 95 95
```

Every value after the first is the expected one plus 96. That is a constant
offset, not a sign-extension difference (which would add 256), so the
emulated run was fed or produced the wrong bytes somewhere between the octal
dump and the executed binary. The agent also never wrote `/app/out.txt`, so
both tests fail on `FileNotFoundError`; `failure_mode` is `unset` and the
exit code is 0, which is the honest record: the run finished, and the answer
was wrong. This is a model result — the investigation was sound and the
reproduction was not verified against anything.

### 2026-11-03 — fifteenth to eighteenth batches, the proxy-dependent tasks

Four more tasks, all of which need something the container could not fetch
before: `huggingface.co` and `youtube.com` are unreachable from a container on
this machine while the host reaches them through its own proxy. They were run
after the adapter grew an opt-in `--task-proxy`, which writes the proxy into
`/etc/profile.d` so the harness's *test* session gets it too — exporting it in
the agent's shell alone left `uv` missing in the test phase and scored a
finished run as `parse_error`.

| Task | Result | Notes |
|---|---|---|
| `hf-model-inference` | ❌ unresolved | batch 15: model saved without `tokenizer.json`; batch 17: timed out mid-`pip install torch` |
| `download-youtube` | ❌ unresolved | batch 16: SSIM 0.948 against a 0.95 threshold, 149 frames against 204 |
| `extract-moves-from-video` | ❌ unresolved | timed out after installing deno and building tooling; `solution.txt` never written |
| `build-initramfs-qemu` | ❌ unresolved | batched, booted, root logged in — but the prompt says `initramfs login:`, not `localhost login:` |
| `build-tcc-qemu` | ✅ resolved | **after the agent timeout fired**, like `cartpole-rl-training` |
| `get-bitcoin-nodes` | ❌ unresolved | 4/5; `/transaction/<genesis txid>` answers 404 while a recent coinbase transaction answers 200 |

The last two were the eighteenth batch, run on the same proxy-enabled setup.

**`download-youtube` is the closest miss here.** YouTube blocked every player
client with "Sign in to confirm you're not a bot", so the agent fetched the
identical transcode from archive.org and cut it with `ffmpeg -sseof -10`. The
result is the right video at the right resolution and frame rate; the last ten
seconds are offset by about four frames, which lands at 0.948 similarity
against the 0.95 threshold. This is a model result worth recording precisely
because it is not a failure of understanding.

**`build-initramfs-qemu` failed on the hostname.** The test's expect script
waits for `localhost login: ` with `set timeout -1`; the agent's init script
never sets the hostname, so the prompt reads `initramfs login:` and the expect
never matches — the test hung until the 900 s test budget expired, which is
recorded as `test_timeout`. Everything the instruction asks for does work: the
kernel boots, root logs in with no password, and `dmesg` carries the custom
`printk` line. The test checks for a detail the instruction does not state.

**A note on the sixteenth batch's artifacts.** The run's `target/` output
directory disappeared part-way through it, so neither trial's `results.json`
survived and the batch produced no score — the two containers were still
running when the output path was already gone. The seventeenth batch repeated
both tasks against `/Users/lando/tb-runs` and produced the results above. No
repository file was lost; only that run's logs were. Output directories for
future batches go outside the repo for that reason.

**`build-tcc-qemu` is the second timeout-and-still-pass.** The kernel is
pre-built in the image; the agent spent the budget producing and testing the
`tcc` ISO, and the timer fired while it was still verifying in QEMU. All of
that work survived, the test passed, and the trial resolved — the run is
recorded as `agent_timeout`, which is now a description rather than a verdict.
Same shape as `cartpole-rl-training` in the fourteenth batch, and the second
piece of evidence that reporting a cutoff honestly costs nothing.

**`get-bitcoin-nodes` fails on one endpoint out of five.** The service is up,
`/status`, `/block/<hash>` (including the genesis transaction ID) and the
error handling all pass. The failing assertion asks
`/transaction/4a5e1e4b…` — the genesis coinbase — and gets 404, while the
agent's own verification, against a *recent* coinbase transaction, returned
200 with `vin[0].coinbase` and `vout[0]`. The agent pointed the block endpoint
at a public RPC node; the genesis transaction is a special case for most
indexed backends because it is not spendable and is often omitted from a
txindex. Not verified further — the trial's service is gone — but the failure
is narrow and reproducible from the transcript.

**`oom` and `eval-mteb` did not build here.** `oom`'s image installs
`transformers` and `torch`, and `pip` gave up after 20 minutes at 62 kB/s
(`ReadTimeoutError` from `files.pythonhosted.org`) with and without the proxy
build args. `eval-mteb`'s apt stage stalls on `libx11-6` from
`deb.debian.org`. Both stay in the never-run list; neither says anything
about Codape.

### 2026-11-03 — nineteenth batch, two fixtures that could not run

The nineteenth batch took `build-linux-kernel-qemu` and `reshard-c4-data`, the
two most expensive tasks still untried. Neither produced a score, and in both
cases the reason is the fixture, not the agent.

| Task | Result | Notes |
|---|---|---|
| `build-linux-kernel-qemu` | ❌ unresolved | `agent_timeout`; test file crashes at import (`TypeError: bad operand type for unary +: 'str'`) |
| `reshard-c4-data` | ⚪ not scored | image build fails on `load_dataset("allenai/c4")` — `huggingface.co` unreachable from the builder |

**The kernel test file does not parse.** `tests/test_outputs.py` line 37 ends
with `"... libssl-dev """` and line 38 begins `+"""libelf-dev ...`. That is
string concatenation split at a line boundary where the operator landed at the
start of the next line, which Python reads as unary plus on a string:

```
TypeError: bad operand type for unary +: 'str'
```

pytest cannot collect the file, so the trial fails regardless of what the
container contains. The agent's own run is a different story: 52 tool calls,
all of them waiting. `apt-get install` of the kernel build dependencies
(`build-essential`, `qemu-system`, `expect`, ...) ran against
`ports.ubuntu.com` for the whole 3000 s budget at roughly 30-60 kB/s and never
finished; the agent polled it every 55 s with `sleep 55; tail -1
/app/apt-install.log`, which is the right patience but the wrong speed. The
kernel tarball itself downloaded fine (144 MB in under a minute). This is the
slow-apt class already recorded for `pytorch-model-cli` and `eval-mteb`, and it
is why the task cannot be scored here — but the fixture bug means even a
finished install would have failed.

The one-character fix is to join the two lines with the operator at the end of
the first (`"""...libssl-dev """ +`), which is what the pristine file meant.
With that, `ast.parse` succeeds. Recorded under [Local fixture
patches](#local-fixture-patches) and re-run in the twenty-first batch.

**`reshard-c4-data` never started.** Its `Dockerfile` runs `python setup.py`
at build time, which calls `load_dataset("allenai/c4", ...)` and downloads
`en/c4-train.00000-of-01024.json.gz` from `huggingface.co`. From inside the
builder that address is unreachable (`[Errno 101] Network is unreachable`,
five retries, then `ConnectionError: Couldn't reach 'allenai/c4' on the Hub`).
The harness records the compose failure as `unknown_agent_error` with
`is_resolved: null` and no trial timings, because no container ever existed.
Unlike the earlier huggingface tasks this one fails in the *image build*, so
`--task-proxy` cannot help: the proxy is installed into a running container,
past the point this fails.

The same `ARG`-plus-compose-`args` patch used for `incompatible-python-fasttext`
fixes it. A direct build with the four proxy build args completed in 27
minutes (the 800 MB C4 shard dominates) and produced an image whose
`/app/c4_sample` holds 9898 JSONL files totalling 799 MB — the state the task
expects. Patched in the fixture and re-run in the twenty-first batch. The
27-minute build means this task cannot be batched casually; the image is now
cached as `tb-probe-reshard:patched`.

### 2026-11-03 — twentieth batch, the hint variant, **1/1 resolved**

`incompatible-python-fasttext.base_with_hint` is the same task with the
instruction pointing at `model.bin` and forbidding new packages. It had never
run for the same huggingface reason as its parent; with the proxy patch it
built and the agent solved it 3/3.

| Task | Result | Notes |
|---|---|---|
| `incompatible-python-fasttext.base_with_hint` | ✅ resolved | 3/3; 4 minutes of agent time; three edits to `FastText.py` |

The agent's path is short and correct: it looked for `model.bin` (the hint's
one addition), found `copy=False` with Grep, then edited three lines of
`fasttext/FastText.py` to use `np.asarray` instead of `np.array(..., copy=False)`
— the same fix the task's `solution.yaml` prescribes. It did not install
anything, which the instruction required and the third test
(`test_site_packages_not_overly_changed`) enforces.

The image build is the slow part, not the task: 25 minutes for the 106 MB
model plus the `fasttext` wheel. Once the build cache holds it, a re-run costs
seconds.

### 2026-11-03 — twenty-first batch, two patched fixtures, **0/2 resolved**

The two tasks the nineteenth batch could not score, re-run against the patched
fixtures.

| Task | Result | Notes |
|---|---|---|
| `reshard-c4-data` | ❌ unresolved | 1/2; `test_revert` fails — the agent nested `c4_reshard/` into `shard_XX/part_XXX/`, which the test's flat `os.listdir` can never accept |
| `build-linux-kernel-qemu` | ❌ unresolved | `agent_timeout` at 3000 s; test ran but `expect` was never installed — because **my patch was wrong** (see below), not because of the task |

**`reshard-c4-data` is a real instruction-versus-test gap.** The instruction
asks for at most 30 files per folder and at most 15 MB per file. Every one of
the 9898 files is about 300 KB, so only the folder limit bites, and the agent
solved exactly that: it built `c4_reshard/shard_XX/part_XXX/<original files>`
with at most 30 entries per directory, wrote `manifest.json`, verified all 9898
sha256 sums, and wrote `revert.py` next to `c4_sample/` — which then restored
9898/9898 files with *"all checksums verified OK"*.

The test requires something else: after `uv run revert.py`, `os.listdir("/app/c4_reshard/")`
must equal the 9898 original basenames. A nested layout cannot satisfy that no
matter what `revert.py` does, and the agent's `revert.py` restored into
`./c4_sample` (a defensible reading of *"revert … back to the original state"*)
rather than into `c4_reshard/` itself, which is what the reference solution
does — it writes the originals back into `/app/c4_reshard/` and deletes the
shards.

The test also has a units bug that makes one constraint vacuous:
`MAX_SIZE_PER_FILE = 15 * 1024 * 1024 * 1024` is 15 **GB**, not the 15 MB the
instruction states. Harmless here because no file is near either bound, but it
means the file-size half of the task is never checked.

So this belongs in the instruction-contradicts-test class: the instruction
never says the resharded folder must stay flat or that `revert.py` must write
back into `c4_reshard/`. Recorded as an unresolved task.

The image build, once patched, took 27 minutes and produced
`/app/c4_sample` with 9898 files — so the fixture is sound apart from the
contradiction above.

**The kernel fixture collected this time, and my first patch was wrong.** The
`TypeError` was gone and pytest ran `test_expect`, which builds its own
initramfs from the `init` string and drives QEMU with an expect script. That
script failed with `init.sh: line 4: libelf-dev: command not found` and then
`/bin/sh: 1: expect: not found`, so no kernel output was ever produced. The
cause was my edit, not the task: I split the concatenation onto a new line,
which inserted a newline inside the `apt-get install -y ... libssl-dev`
command and broke the list in two. The correct patch keeps the two literals on
one line with their original spacing:

```python
apt-get install -y build-essential libncurses-dev bison flex libssl-dev """+"""libelf-dev qemu-system bc cpio wget expect
```

which concatenates to `... libssl-dev libelf-dev ...` — exactly the single
command the file meant. Verified by `ast.parse` and by evaluating the literal.
The agent side of the run was healthy: 168 transcript frames, apt for the
build dependencies finally completed after ~30 minutes, the 6.9 tree extracted,
`make` was running when the 3000 s timer fired. Re-run in the twenty-second
batch with a 7200 s budget.

This is worth recording as a caution: a fixture patch that makes a file
*import* is not the same as one that preserves its behaviour, and this test
executes the patched string rather than just containing it.

### 2026-11-03 — twenty-second and twenty-third batches, the kernel and fasttext training

| Task | Result | Notes |
|---|---|---|
| `build-linux-kernel-qemu` | ❌ unresolved | `test_timeout`; the agent built `bzImage` and the initramfs in 7200 s, then the *test's own* apt-get ran out of its 1800 s before QEMU started |
| `train-fasttext` | ❌ unresolved | `agent_timeout`; accuracy reached 0.6233 inside the size limit, but no `/app/model.bin` was ever written |

**The kernel task now gets all the way to a built kernel.** With the corrected
string and a 7200 s budget, the agent installed the toolchain (apt took ~25
minutes from `ports.ubuntu.com`), extracted linux-6.9, added
`printk(KERN_INFO "Hello, this is a custom kernel")` to `init/main.c`, built
`arch/x86/boot/bzImage`, and produced `ramfs/initramfs.cpio.gz`. The agent
finished; the *test* then timed out.

The test's `init` string is a full provisioning script: it re-runs
`apt-get update` and reinstalls `build-essential`, `qemu-system`, `expect`,
`bc`, `cpio`, `wget` from scratch before booting anything. On this network that
takes far longer than the 1800 s test budget, so QEMU never ran and the trial
is recorded as `test_timeout`. The reference task assumes a fast apt; here the
same 25 minutes the agent already paid are paid again. That is a measurement
artefact of the network, not a Codape result — the deliverable it grades was
present.

The remedy is to pre-install the toolchain in the fixture image, so the test's
`apt-get install` is a no-op and the test measures the kernel rather than the
package mirror. That build is running.

**`train-fasttext` — right answer, missing file.** The agent converted the two
parquet files to fasttext format, trained a 848 MB model, and then quantised it
five ways to fit the 150 MB limit. Two candidates cleared 0.62:

```
A_cut1M_noretrain: acc=0.6233 sizeMB=59.7
B_cut1M_retrain:   acc=0.6187 sizeMB=59.7
```

`A_cut1M_noretrain` meets both requirements — 0.6233 against a 0.62 floor,
59.7 MB against a 150 MB ceiling. But the run shared its 2700 s budget with
the 495 MB text conversion and a 1.29 GB base training pass, and the timer
fired while candidate `C_cut1.5M_noretrain` was still quantising. `model.bin`,
the one thing the instruction asks to be saved, was never copied into place, so
both tests fail on a missing file rather than on model quality. The work to
produce a passing model was done; the last step was not. Recorded as a
budget/model-behaviour failure, not a fixture problem.

The fixture needed the same proxy patch as the fasttext tasks — its Dockerfile
`wget`s two Yelp parquet files from `huggingface.co` at build time — and with
it the image builds (477 KB test split, 299 MB train split).

### 2026-11-03 — twenty-fourth batch, the kernel boots the wrong initramfs, **0/1 resolved**

| Task | Result | Notes |
|---|---|---|
| `build-linux-kernel-qemu` | ❌ unresolved | `test_timeout`; with the toolchain pre-installed the agent finished in 24 minutes and the test booted — but it booted a ramfs that never reaches a login prompt |

With the toolchain in the image the test's apt-get became a no-op and QEMU
finally ran. It still failed, for a reason that had nothing to do with Codape.
The instruction says:

> I've created an initial ramfs for you, so once its built, all you should have
> to do is run `qemu-system-x86_64 -kernel ./linux-6.9/arch/x86/boot/bzImage
> -initrd ramfs/initramfs.cpio.gz …`

No such ramfs shipped. `ls -la /app` in both the patched image and the pristine
`ghcr.io/laude-institute/t-bench/ubuntu-24-04:latest` is empty, so a compliant
agent has to build one — the agent did, and `poweroff` there would have worked.
Then the test's own provisioning script ran in the same directory:

```
mkdir: cannot create directory 'ramfs': File exists
gzip: initramfs.cpio.gz already exists; not overwritten
gzip rc=2
```

`gzip` exits 2 rather than overwriting, and the test has no `set -e`, so it
carried on and booted **the agent's** initramfs with the test's own
`bzImage` path. That initramfs ends with `exec /linuxrc`, and busybox's
`--install` does not create `/linuxrc` at the filesystem root in this
configuration, so init failed and QEMU idled at 0.5 % CPU until the 2400 s test
budget expired — recorded as a test timeout with no output at all.

Three fixture defects, all upstream of the agent: a promised file that never
existed, a test script that silently reuses an agent's artifact, and an
unbounded `set timeout -1` that turns any boot failure into silence. None of
them is a Codape defect.

### 2026-11-03 — fixing the kernel fixture's ramfs

The fixture now supplies the artifact the instruction promises, and its test
provisions itself idempotently:

- **The image builds `ramfs/initramfs.cpio.gz`.** It uses the same `basic-init`
  and the same busybox binary as the test's `initramfs.list`, with the same
  layout — including `nod /dev/console c 5 1`, which the first draft omitted.
  That omission is fatal: without the console node PID 1 has no stdio and the
  boot stops silently after "Starting up...". `gen_init_cpio` needs the kernel
  source, which does not exist at image-build time, so `cpio --format=newc`
  packs the tree; both emit newc archives and the kernel cannot tell them
  apart.
- **The test's `mkdir ramfs` is `mkdir -p`**, and its `gzip -9 initramfs.cpio`
  is preceded by `rm -f initramfs.cpio.gz`. The test now always boots the ramfs
  it just built instead of whatever the agent left behind.
- **The test's expect script is bounded.** `set timeout -1` became 600 s for
  the boot and 120 s for each prompt, and every `expect` reports which stage
  failed (`BOOT TIMEOUT: no login prompt`, `NO SHELL PROMPT AFTER LOGIN`, …)
  and exits non-zero. A boot that fails now says so; a boot that is merely slow
  still gets the budget.

Verified before re-running: the new image's archive lists
`dev/console`, `root` (0700), `init`, `bin/busybox`; booting it under
`qemu-system-x86_64` with a throwaway x86_64 kernel reaches
`localhost login:`, `root` logs in, `dmesg | grep Hello` returns to a prompt,
and `poweroff` shuts down cleanly. Before the fix the same boot hung after
`Starting up...`.

The patch is local to the fixture, in the same `.orig`-sibling style as the
other edited tasks: `Dockerfile` and `tests/test_outputs.py`. This fixture has
no compose change — unlike the fasttext tasks it does not fetch anything the
proxy is needed for at build time, and its `ARG` declarations stay unused on a
direct apt route.

### 2026-11-03 — twenty-fifth batch, the fixture fix verified, then the provider ran out

| Task | Result | Notes |
|---|---|---|
| `build-linux-kernel-qemu` | ❌ unresolved | `unknown_agent_error`: Kimi's weekly quota was exhausted mid-build; the test still completed in 40 s and said exactly what was missing |

The fixture fix worked as intended. The agent found the shipped
`ramfs/initramfs.cpio.gz`, downloaded linux-6.9, ran `make defconfig`, enabled
`BLK_DEV_INITRD` / `DEVTMPFS` / `TTY` / `SERIAL_8250_CONSOLE`, and started the
cross build — then the provider stopped the run:

```
API error (429): You've reached your weekly (7-day) usage limit.
```

724 s of work, `is_error: true`, no kernel. Nothing in this trial is a Codape
or fixture result.

What it did prove is the diagnostic half of the fix. The old test would have
spent 2400 s booting a kernel that does not exist and reported only
`test_timeout`. Instead the test phase finished in **39.5 s**:

```
could not open kernel file './linux-6.9/arch/x86/boot/bzImage'
AssertionError: Expected 'Hello, this is a custom kernel' to be in post-agent.txt
```

The expect script exits non-zero on EOF at the first prompt, so the harness
gets a named failure instead of silence. That is the behaviour the twenty-fourth
batch lacked.

Kimi's 7-day quota is now spent, so no further KIMI batches can run until the
window resets. The kernel task still needs one clean run to be scored.

### 2026-11-03 — the proxy is not free, and `--task-proxy-direct-hosts`

The kernel task exposed a cost the earlier proxy work had hidden. On this
machine, the same `apt-get install` of the kernel toolchain took:

| Route | Time for `build-essential libncurses-dev bison flex libssl-dev libelf-dev qemu-system bc cpio wget expect curl busybox-static` |
|---|---|
| direct | **66 s** |
| through `http://host.docker.internal:1087` | **> 20 min** (not finished when the build was killed at 19 min) |
| through the proxy, with the apt hosts in `no_proxy` | **68 s** |

Measured in `ghcr.io/laude-institute/t-bench/ubuntu-24-04:latest`. So the proxy
that makes `huggingface.co` reachable makes `ports.ubuntu.com` roughly twenty
times slower, and the earlier batches that used `--task-proxy` on tasks doing
real `apt-get` work were paying that without noticing.

The adapter now accepts `--task-proxy-direct-hosts`, a comma-separated list
of hosts appended to `no_proxy` / `NO_PROXY` in both the agent's environment
and `/etc/profile.d/99-codape-proxy.sh`. With
`--task-proxy http://host.docker.internal:1087 --task-proxy-direct-hosts
ports.ubuntu.com,deb.debian.org,pypi.org`, apt stays on the fast direct route
while `huggingface.co` still goes through the proxy. The provider host and
loopback are always direct regardless, as before.

The no-proxy string is composed once in `_no_proxy_value()` and used in both
places, so the agent shell and the test shell cannot disagree. Verified by
construction:

```
with direct: localhost,127.0.0.1,::1,host.docker.internal,ports.ubuntu.com,deb.debian.org,api.kimi.com
without    : localhost,127.0.0.1,::1,host.docker.internal,api.kimi.com
```

For the kernel task this turned out not to be enough on its own, because its
*test* re-runs the whole toolchain install and the test budget is per-task;
the image pre-install below is the actual fix there. But for any task that
installs packages and also needs a blocked host, this is the correct shape of
the flag.

### 2026-11-04 — all thirty-one unresolved tasks, re-run on Linux

The earlier batches ran on macOS through a proxy, several of them against KIMI.
This campaign replaced the machine and the endpoint. The host is Linux 6.8 on
x86_64 (16 vCPU, 61 GB RAM, Docker Engine, a 50 GB root filesystem), the
network is direct, and the model is `deepseek-v4.1-flash` behind
`https://opencode.ai/inference/go/anthropic`, with
`CODAPE_CONTEXT_WINDOW=131072` and the header `x-opencode-session`. The agent
binary is Codape 0.1.3 built for `x86_64-unknown-linux-musl` — static-pie and
stripped, so it runs in every task container without a libc. None of these runs
used `--task-proxy`: the hosts that were only reachable through one on the Mac
resolve directly here, which also removes the 20× apt slowdown the proxy
introduced.

Every task the previous scoreboard listed as unresolved was run again against
`0.1.1-analysis`. The first pass ran 24 of the 31 in four concurrent groups —
three at a 600 s agent budget and one at 900 s — and resolved 11. The seven
tasks that did not fit and the thirteen first-pass failures were then rerun
with budgets matched to the failure (900 s to 2400 s), one to four at a time,
after diagnosing each one.

The 50 GB root filled twice as images and build caches accumulated. Two trials
died with `OSError: [Errno 28] No space left on device` while the harness wrote
`post-test.txt`; one of them, `hf-model-inference`, had already finished its
agent phase. `docker builder prune -af` recovered 13.4 GB. The harness warns
that `docker buildx` is unavailable, so it cannot prune build cache per trial
and disk use has to be watched by hand during a sweep.

#### What the failures turned out to be

Twenty of the 26 tasks that now pass were failing on the fixture, not on the
model. The most common shape is an instruction that omits exactly the fact the
test checks:

- `build-initramfs-qemu`: the expect script waits for `localhost login:`, but
  the instruction never sets a hostname.
- `configure-git-webserver`: the sample commands say `user@server`, `https`,
  `index.html` and branch `webserver`; the test wants `git@localhost`, HTTP,
  `hello.html` and `master`.
- `decommissioning-service-with-sensitive-data`: the test requires the archive
  to contain `opt/sensitive_service_data/...`; the instruction says only "the
  entire directory".
- `git-multibranch`: the test requires an initially empty repository on
  `master` with no leftover test branches; the instruction never says so.
- `jupyter-notebook-server`: the test reads
  `~/.jupyter/jupyter_notebook_config.py` and looks for `c.NotebookApp.*`
  settings; the instruction names no file and no keys.
- `nginx-request-logging`: the test reads the site file for `listen 8080` and
  `root /var/www/html`, but reads `nginx.conf` for `log_format` and
  `limit_req_zone`; the instruction puts every directive in the site file, so
  the second read finds nothing.
- `polyglot-c-py`: the instruction says `main.c.py`, the test runs
  `main.py.c`.
- `raman-fitting` / `raman-fitting.easy`: the test expects `gamma` to be the
  Lorentzian HWHM and the amplitude key to be `A`, and fits each peak in a
  fixed window; the instruction only says "fit the peaks".
- `reshard-c4-data`: the test requires a flat `c4_reshard/` holding the
  original basenames after `revert.py`; the instruction bounds files per
  folder and says "matches what was originally in `c4_sample`", which a nested
  layout satisfies.
- `sanitize-git-repo` / `.hard`: the test forbids editing the downloaded
  `exp_data/` snapshots; the instruction only says "do not make unnecessary
  changes". The hard variant additionally requires the pre-sanitization
  history to remain resolvable by its original SHA.
- `vim-terminal-task`: the test executes `text_processor.py`; the instruction
  never asks for the executable bit.

The rest are test defects rather than instruction gaps:

- `cron-broken-network`: the test compared the served page against 90 % of
  example.com's old body; example.com has since been redesigned, so a correct
  cron job fails. It now asserts `Example Domain` is present.
- `intrusion-detection`: the test grepped the source for the literal
  `logs/auth.log`; the script uses a variable and works.
- `conda-env-conflict-resolution`: the task claimed NumPy 1.22 is incompatible
  with SciPy 1.9.0. scipy 1.9.0 declares `numpy>=1.21.6,<2.0`; conda solves
  `numpy=1.22.0` with `scipy=1.9.0` and both import. The test demanded a
  change that is not needed. It also grepped comments, so quoting a removed
  pin in a comment failed it.
- `hf-model-inference`: the test required `vocab.txt`; `save_pretrained` wrote
  `tokenizer.json`, and the instruction never prescribes a serialization. The
  API tests had already proved the directory loads.
- `qemu-alpine-ssh`: `debian:bullseye-slim` now resolves a qemu package that
  was deleted upstream; the image no longer builds. The test's ssh command
  also concatenated `"StrictHostKeyChecking=no"` and `"-p 2222"` without a
  space, so it ran `ssh ...no-p 2222`.
- `build-linux-kernel-qemu`: `tests/test_outputs.py` joined an `apt-get`
  string across lines, leaving `+"""libelf-dev` as unary plus on a string, so
  pytest could not collect the test at all. Once it parses, the image ships no
  `ramfs` though the instruction promises one, and the unguarded `gzip` then
  booted the agent's archive.
- `reshard-c4-data`: the test's size limit was `15 * 1024 * 1024 * 1024` — 15
  GiB, not the 15 MB stated everywhere else.

The pytorch case is the clearest, because the fixture was forcing the model to
be *less* accurate. `TRUE_PRED_CLASSES` was generated from the reference
solution, which feeds the PNG's `p/255` values straight into the network. The
test generates its images with `transforms.ToTensor()`, and the obvious
reading of "run the model" applies the model's mean/std normalization.
Measured over the first 50 MNIST test images with a restricted unpickler that
reads only the archive's tensors:

| Preprocessing | Matches `TRUE_PRED_CLASSES` | Correct against true labels |
|---|---|---|
| `p/255` (reference) | 50 / 50 | 35 / 50 |
| `(p/255 - 0.1307) / 0.3081` | 40 / 50 | **43 / 50** |

The four indices the first Linux run failed on — 28, 32, 34, 44 — are exactly
places where the true labels are 0, 3, 7, 3 (the agent's answers) while the
expected list says 3, 8, 2, 8. The expected list encodes the reference's own
15 mistakes. Naming `model.pth` and stating `p/255` with no normalization
makes the task well-posed; the rerun passes all 6 tests.

#### The other six were budget, plumbing, or the new network

Six tasks passed on a rerun with no fixture change: `count-dataset-tokens`
(huggingface.co is reachable directly now), `download-youtube` (SSIM 0.948
against a 0.95 threshold the first time, a boundary result), `get-bitcoin-nodes`
(the earlier failure used a live block explorer), `path-tracing-reverse`
(passed at 602 s of a 900 s budget), `polyglot-rust-c` (283 s), and
`tmux-advanced-workflow` (the earlier grade starved on `apt-get update`; the
`parse_error` was in grading, not in the agent's work).

#### Budgets

Agent budgets were 600 s for the short groups, 900 s for data/download tasks,
1200–1800 s for builds and services, and 2400 s for training. `--max-turns` was
200 everywhere; test budgets were 120–900 s; the context window was 131072.

The per-request *output* ceiling is separate: Codape's
`DEFAULT_MAX_TOKENS = 32768`. At the time of these runs the adapter had no
`max_tokens` option and the CLI had no flag — only an agent definition could set
one (`set_max_tokens`), which these runs did not use. `CODAPE_MAX_TOKENS` now
covers that gap; the runs below predate it and ran at 32 768.
`gpt2-codegolf`'s first run is the case where this
mattered: it ended after 202 s with `error_max_output_tokens`, one turn, zero
tool calls, because reasoning consumed all 32 768 tokens before any text or
tool use. Probing the provider directly with `max_tokens` 65 536 and 131 072
returned HTTP 200 and `end_turn` for both, so 32 768 is Codape's default, not
a gateway cap. At 900 s the second run wrote `/app/gpt2.c` at 5156 bytes
against the 5000-byte limit and was then cut off by the agent timeout.

A third run, at 1800 s, went the other way. It spent 44 turns and 46 tool calls
mapping the checkpoint format — the transcripts are all probes of the `.ckpt`
and scratch C files under `/app/w/` — and never created `/app/gpt2.c`. It
stopped at 979 s with `{"subtype":"success","is_error":false,"result":""}`.
That closing turn carried no text and no tool call; the transcript has no
assistant text at all, and the last frame before the result is a tool result.
The only response shape consistent with both facts is a thinking-only turn: a
text delta or a tool call would have produced a frame. Codape reported it as a
finished run, which is defect 12 below.

A fourth run, with the empty-answer fix in place (1800 s), did not reproduce
the empty turn. It made 89 turns and 90 tool calls and did produce
`/app/gpt2.c` — under the 5000-byte limit, and it compiled — but the program
segfaulted with `returncode -11` on the test input. The 1800 s timer ended the
run while it was still fixing that, and the frame is the honest
`subtype: interrupted` with `error: "SIGTERM ended the run before the turn
finished"`. The guard was not exercised by this run; a local mock provider is
what verifies it end to end (see defect 12).


### 2026-11-04 — `train-fasttext`, the model was ready and the disk was not

A rerun of `train-fasttext` (run `fasttext-ship`) added the instruction that
the earlier attempts lacked: save a model to `/app/model.bin` as soon as it
meets the bounds, then keep improving. The agent did. The run still produced no
score, for two reasons that are not model quality.

| Time (UTC+8) | Event |
|---|---|
| 20:05 | the trial's own `df` reads 3.4 GB free (93 % used) |
| 20:07 | `final_train.py` starts in the background: dim 100, bucket 2 M, wordNgrams 2, epoch 5, quantised (`cutoff=200000`, `dsub=8`), `out=/app/model.bin` |
| 20:08 | a qualifying model lands: `model.bin` 4 637 279 B, `acc_test` 0.6235, quantised 0.624, val 0.6606 — inside the 150 MB and >0.62 bounds |
| 20:09 | it copies the model to `model_quant_backup.bin`; a Python check of it crashes on `fasttext.predict` under NumPy 2.5.4 (`np.array(..., copy=False)`) |
| 20:09–20:21 | it keeps searching: alternate splits score 0.6275 and 0.617; `dim=200` 0.618; `bucket=8M` 0.6164 — none beats the saved model |
| 20:21 | it writes a/b train splits of 461 MB and 462 MB and launches `run_g11.sh` with `bucket=8_000_000` in the background |
| 20:22:51 | the 3 600 s timer fires: `subtype: interrupted`, `num_turns: 110`, exit code 124 |
| 20:22:51 | the harness writes `panes/post-agent.txt` and raises `OSError: [Errno 28] No space left on device` (`harness.py:750`) |
| — | `results.json`: `failure_mode: unknown_agent_error`, `parser_results: null`, all timestamps null; the tests never ran |

The recorded failure mode is wrong in a way worth remembering. The adapter maps
`interrupted` to `AGENT_TIMEOUT`, and `_run_trial` deliberately continues to
the tests for that mode. The pane write raised before the mode was recorded and
the exception left the trial, so the row survived only as a null-filled
`unknown_agent_error`. Had the write succeeded, the tests would have graded the
4.6 MB model that was last written to `model.bin` — above the 0.62 floor by
0.0035 on the local 10 k test split, so plausibly a pass but not a safe one.

The disk was the agent's own. fastText writes an unquantised model of roughly
`(vocab + bucket) × dim × 4` bytes, so `bucket=2M, dim=200` is about 1.6 GB and
`bucket=8M, dim=100` about 3.2 GB. The trial's own `df` readings ran from
3.4 GB to 5.4 GB free, and it spent that on duplicate corpora (482 MB,
469 MB, then a 1.38 GB `p3` file, later deleted) and GB-scale candidates; by
20:22 the pane could not be written.

Two model-behaviour lessons come with it:

- **The missing deliverable became a missing stop.** The twenty-second batch
  failed because no `model.bin` was written; this run fixed that and then spent
  the remaining 14 minutes on candidates that never beat it. It used 110 turns
  and 47 `sleep` polls, and was still launching jobs when the timer fired; a
  deadline-aware stop is what it lacked.
- **The NumPy 2.5 self-check is a trap.** The task container has NumPy 2.5.4,
  where `fasttext-wheel`'s `predict` raises `ValueError: Unable to avoid
  copy`; the grading environment pins `numpy==1.24.0`. The agent distrusted its
  CLI evaluation after its Python check crashed, and went back to training.

#### The same disk ended the pdp11 rerun

The `run-pdp11-code` rerun on the defect-14 fix (3 600 s) produced no result
either. At 28 minutes its container's writable layer was **17 GB**, almost all
under `/app/work`, and the root filesystem was at 0 bytes. Removing the
container was the only way to recover the host; the launcher then died writing
its own output with `OSError: [Errno 28]`, `results.json` is 0 bytes, and
`post-agent.txt` is empty. The run cannot say whether the fix works and needs a
rerun with a disk guard. The pre-fix pdp11 run remains the evidence for defect
14.

The 17 GB is a model artifact, not Codape: its last visible tool result shows
a Python PDP-11 emulator executing 5 669 instructions and printing the wrong
sequence, and `/app/work` was still being rewritten as the disk filled. Container
writable layers are on the host's root filesystem, so a long trial needs a
periodic `df`/`du` check as well as the preflight one.

A second casualty of the same period was not a result at all:
`playzork-budgetfix-600turns` started two minutes after the fasttext ENOSPC,
ran its agent phase for 24 s, wrote no `codape-stream.jsonl`, and its
`post-agent.txt` is `can't find pane: agent`. It is recorded as invalid and
says nothing about the budget fix.
A fifth run, at 7200 s, was the first to reach the compaction defect: it died
after 41 turns and 934 s with the circuit breaker open while the provider had
accepted every request (defect 13). A sixth run, on the escalation fix, got
past that point and continued, but then stopped at 18.5 minutes with the
context at the blocked line; a debug-instrumented run showed the trigger
measuring 99 242 tokens while strategy selection counted 72 389 — two rulers
for one decision. The seventh run, on the binary with both fixes, **resolved
the task**: 116 turns and 2 597 s, with two `compact_boundary` frames in the
transcript showing that compaction ran twice and the run continued. It wrote a
4 979-byte `/app/gpt2.c` that builds with the user's exact command, and its
final message reports 56 of 57 reference prompts matching HF GPT-2
byte-for-byte.

### 2026-11-05 — reruns, and the first runs of two never-run fixtures

The four tasks the 2026-11-04 campaign left unresolved were rerun on the binary
built from `dev` with the defect-14 fix and the provider-ruler fix. Two
never-run fixtures were also tried for the first time, `security-vulhub-minio`
and `solana-data`; both now have a real trial, though neither resolves. The
reruns got 3 600 s and 200 turns unless noted; `run-pdp11-code` ran against the
analysis fixture whose instruction now asks for `/app/out.txt`.

| Task | Result | Notes |
|---|---|---|
| `train-fasttext` | ❌ unresolved, clean | 27 turns, 766 s, `failure_mode: unset`; size passed, private accuracy **0.601275** against 0.62 |
| `play-zork` | ❌ unresolved | `agent_timeout` at 3 600 s, 239 turns, 5 compactions, `/app/answer.txt` never written |
| `run-pdp11-code` | ❌ unresolved | run 1: `error_max_output_tokens` at 39 turns; run 2: 133 turns / 3 600 s, no compaction, a PDP-11 emulator whose output was close but wrong; no `/app/out.txt` either time |
| `extract-moves-from-video` | ❌ unresolved, environment | 600 s `agent_timeout`, 34 turns; `yt-dlp` got "Sign in to confirm you're not a bot" and every mirror answered 403/500/502 |
| `security-vulhub-minio` | ❌ unresolved, model | 25 turns, 231 s, `failure_mode: unset`; it proved the access key and gave up on the secret without trying `POST /minio/bootstrap/v1/verify` |
| `solana-data` | ❌ unresolved, timeout | 31 turns, 360 s, `agent_timeout`; the Flask server reached `/status: 200` during the run but was not listening when the tests started |

**`train-fasttext` is now a straight model result.** The run had no disk or
harness problem: 766 s of agent time, 27 turns, `subtype: success`, and both
tests executed. `test_model_size` passed at 123.9 MB; `test_accuracy` failed on
the private split:

```
E       assert 0.601275 > 0.62
```

The agent's own held-out 10 000-row split said 0.6274, which is why it stopped
("Bounds met (0.6274 accuracy, 124MB)"). The winning configuration was
`dim=10, bucket=2_500_000, wordNgrams=2, minCount=2, epoch=3, lr=0.5,
loss=softmax` — capacity was deliberately small because larger settings
overfitted (`epoch 25` scored 0.580). The 2.6-point gap between the provided
holdout and the private split is the whole story: the model selected its
hyperparameters against the only labelled data it was given, and that data is
not the test distribution. This also explains the earlier 0.6233/0.6235 local
readings — they were measured on the same friendly split. The task needs
held-out robustness, not a better training loop.

That was the reading on 2026-11-05. The follow-up found a simpler cause: the
training data and the hold-out had been lower-cased, and the private test had
not. With a case-preserving hold-out and an early write of `/app/model.bin`,
the task passes; see the 2026-11-06 section below. The 2.6-point gap was
partly a format mismatch, not only a distribution shift; Claude's ablation
puts the case effect at 0.76 points.

**`play-zork` confirms defect 14 and nothing else.** The transcript contains
five `compact_boundary` frames (frames 281, 328, 341, 383, 580) and the run
kept issuing tool calls after each one; no "retry budget is spent" appears.
That is the behaviour the fix was for, and it is now exercised end to end on a
real task. The trial still ended at 3 600 s with 239 turns:

```
{"type":"result","subtype":"interrupted","is_error":true,
 "duration_ms":3599995,"num_turns":239,
 "error":"SIGTERM ended the run before the turn finished"}
```

and both tests fail on `FileNotFoundError: /app/answer.txt`. The time went into
a hand-built toolkit — 224 `Bash` calls, 18 `Write` calls, a `probe.py` that
batched commands (95 runs), `save.sh`/`load.sh` save states (17 and 16), and a
room map. The highest score it printed was 174/350 at move 415, and at the
timer it was still at the Entrance to Hades. This is the model's long-horizon
play, not a compaction defect: 3 600 s is not enough for this model to finish
Zork.

Two details matter for reading the missing file. The agent never mentions
`/app/answer.txt` in any tool input, but that is a consequence, not the
mistake: the test wants the ending text *and* the 350-point score, so the file
can only be written after the game is finished. And the one shortcut it tried
was unavailable — two `WebSearch` calls asked for a Zork walkthrough and both
returned `No results found`, so it had to derive the solution instead of
following one.

**At double the budget `play-zork` finishes — and the test is wrong.** The
rerun with 7 200 s and 1 200 turns (`dev-zork-2`) beat the game: 350 of 350
points, rank Master Adventurer, 604 moves, in 442 turns and 5 912 s of agent
time. It wrote the ending verbatim to `/app/answer.txt`, and
`test_reached_end_of_game` passes. `test_maximum_score` is the only failure,
and it is a string mismatch, not a missing result:

```
E  AssertionError: Expected 'Your score is 350 (total out of 350 points)' to be in answer.txt
E  assert 'Your score is 350 (total out of 350 points)' in
   '... Your score is 350 (total of 350 points), in 604 moves. ...'
```

The game never prints "out of". Every `score` reply in the transcript — forty-two
distinct ones, from 0/350 in three moves to 350/350 in 597 — reads
`Your score is N (total of 350 points)`, and so does the ending screen. The
agent also decompiled the story's `V-SCORE` routine and read the format string
itself: `<TELL " (total of 350 points), in ">`. The expected string comes from
the task's own `solution.sh`, which writes both lines with `echo` instead of
playing; the test file is byte-identical in `0.1.1` and `0.1.1-analysis`, so
this is upstream. The task is tagged `shortcut-solution`, and this is exactly
what that tag buys: the only way to pass is to recite the author's paraphrase,
while following the instruction — "write the ending message ... exactly as it
appears on the screen" — guarantees a failure even at a perfect score. Codape
solved the stated goal; the fixture, not the model, is what failed. Until the
assertion is corrected to the string the game emits, a `play-zork` pass is
evidence about memorised test data, not about play.

This also settles the budget question from the 3 600 s run. Zork at 350 is
reachable in roughly 1 h 40 m by this model, with no compaction involved
(442 turns against a 1 200-turn ceiling, the long session state carried in
the conversation rather than the tool kit); the earlier run was not close
because it spent an hour building probes and save states, not because the
window was too small.

**`run-pdp11-code` did not reach a test, for a new reason.** The fixture patch
worked — the instruction asked for `/app/out.txt` and the agent knew it. It
identified the file as a PDP-11 OMAGIC `a.out`, wrote a disassembler, and was
looking for `apout` when the run ended:

```
{"type":"result","subtype":"error_max_output_tokens","is_error":true,
 "duration_ms":1034280,"num_turns":39}
```

A turn hit the 32 768-token output ceiling with no text and no tool call — the
same thinking-only shape as `gpt2-codegolf`'s first run, now at the ceiling
that was supposed to leave room for reasoning. Codape treats `max_tokens` as
a terminal reason by design (retrying would double the cost), so the whole
session ends. At the time the adapter offered no `max_tokens` knob and the CLI
had no flag; the provider accepted 65 536 and 131 072 in a direct probe. The
`CODAPE_MAX_TOKENS` variable added on 2026-11-09 is the missing knob, and it is
clamped to half the model's context window. Nothing in this
trial is a test result: `/app/out.txt` was never created, and the recorded
`unknown_agent_error` is the harness mapping the failed result frame.

A second rerun reached the tests. It ran the full 3 600 s and 133 turns
(`Bash` 129, `Write` 4, `WebSearch` 3), with **no compaction at all** — the
trial never approached the context limit. It decoded `data.hex`, wrote
`/app/work/cpu.py` (a PDP-11 emulator) and `dis2.py`/`annot.py`
(disassemblers), and was still debugging the CPU when the timer fired:

```
17 8 -14 -7 -54 -77 -39 -20 -10 -55
-28 -64 -32 -16 -8 -4 -2 -1 -1 -1
```

That is the right shape (20 values, the same descending tail) and the wrong
numbers: the expected sequence starts `17 -60 -30 -15 -58 -29 -15 -8 -54 -27`
and has one more `-1` at the end. `/app/out.txt` was never written, so both
tests fail on `FileNotFoundError`; `failure_mode: agent_timeout` is honest.
Compared with the 2026-11-04 run, which was off by a constant 96, this is a
real second, independent attempt — the emulator is closer, but still not
correct at the hour mark. This is a model result (emulator correctness), not
a fixture or harness defect.

**`security-vulhub-minio` is solvable, and the model missed the door.** The
trial finished cleanly (25 turns, 231 s, `failure_mode: unset`) and failed the
single assertion because `/app/result.txt` contained only half the pair:

```
AssertionError: Expected 'minioadmin-vulhub' to be in /app/result.txt
```

The agent's report is a careful piece of work: it found the three-node
cluster, proved `minioadmin` is the access key by distinguishing
`SignatureDoesNotMatch` from `InvalidAccessKeyId`, scanned all 65 535 ports on
each node, checked for a mounted Docker socket, and then concluded the secret
was unreachable "from this container". It never tried the documented
vulnerability of the image it was pointed at. `vulhub/minio:2023-02-27…` is
**CVE-2023-28432**, and the endpoint is right there:

```
POST /minio/bootstrap/v1/verify     (Content-Type: application/x-www-form-urlencoded)

{"MinioEnv": {"MINIO_ROOT_USER": "minioadmin",
              "MINIO_ROOT_PASSWORD": "minioadmin-vulhub", ...}}
```

I reproduced that by starting the fixture's three nodes and sending the
request; the credentials the test wants come back verbatim. So the task is a
knowledge/recall result, not a broken fixture: the agent did not search for the
image's CVE and reasoned itself into a dead end instead. It is not a permanent
result: after the request-level changes described in the next section, three
reruns pass in 124 s, 155 s and 309 s.

**`solana-data` was a timeout on a working server.** The agent built a Flask
server in `/app/server.py` and verified it end to end — the transcript has
`[PASS] /status -> 200` and `port 5000 OPEN`. At 360 s the timer fired while
its last command was `kill_server.py; sleep 1; nohup python3 server.py …`,
restarting the server; the SIGTERM took the process group with it, so when the
tests ran ten seconds later nothing was listening and all five errored with
"Solana service is not responding". Two things to keep separate: 360 s is a
short budget for installing `solana` and writing the server, and a server
started without `setsid` does not survive an `agent_timeout`. The model result
is real but the recorded one is a timeout, not a wrong implementation.

### 2026-11-05 — what the two agents send, and what Codape never told the model

`security-vulhub-minio` failed four times in a row (0/4, including the first
run above) while Claude Code 2.1.318 passed it on the same provider endpoint.
The difference had to be below the fixture, so I put a local mock Anthropic
endpoint in front of both agents and recorded the request bodies they send
(`POST /v1/messages`, plus the `/count_tokens` probe). They share a model and a
transport; the requests differed like this:

| | Codape (before) | Claude Code 2.1.318 |
|---|---|---|
| System prompt | 887 characters, four sentences | ~6 200 characters: autonomy, a security-testing authorization paragraph, action bias |
| Environment | — | a second, ~8 400-character environment description in the system role |
| Tools advertised | 53 | 21 |
| Tool `description` | the bare tool name — `Bash` was described as `Bash` | a paragraph or more each, saying what the tool does and when to reach for it |
| `max_tokens` | 32 768 | 32 000 |
| `thinking` | absent | `{"type": "adaptive"}` |
| `output_config` | absent | `{"effort": "high"}` |

The Claude Code side of that comparison is `cc-vulhub-2` (resolved,
`failure_mode: unset`). `cc-vulhub-1` is `unknown_agent_error` — its agent
shell exited without running the command — and is not a result, and a Claude
Code `train-fasttext` run from the same period (`cc-fasttext-1`) was stopped by
the disk guard at 3 GB free (container exit 137), also not a result. Its
rerun, `cc-cmp-fasttext-2`, did pass; it is recorded in the 2026-11-06
section.

The description row was a Codape bug, not a design choice.
`ToolDescriptor` carries only a name, a tier and JSON schemas;
`RegistryToolDispatcher::list_tools()` looked for a `description` key inside the
tool's input schema and then fell back to `d.name`. None of the builtins put one
there, so all 53 tools went to the provider described as their own names. The
model could see each argument schema but nothing said what the tool was for or
when to use it — and nothing in the system prompt framed the agent's job as
"keep going until the task is done" or gave the security-testing authorization
paragraph Claude Code sends.

Two controls before concluding the framing was the cause:

- **The model knows the answer.** Asked with a minimal prompt about a host
  running `vulhub/minio:2023-02-27`, it named CVE-2023-28432 and
  `POST /minio/bootstrap/v1/verify` unprompted. The failure was not knowledge.
- **Text appended to the prompt did nothing.** `dev-vulhub-3` appended only
  Claude's security paragraph with `--append-system-prompt`, and `dev-vulhub-4`
  appended its whole lean body. Neither run made a single
  `bootstrap/v1/verify` request; both repeated the same local env/file probing
  and the same wrong conclusion. Reading `build_headless_prompt` explained it:
  in `-p` mode the append is folded into the *user* prompt as
  `Additional system instructions: …`, and the system prompt stays the
  887-character default. An operator cannot replace Codape's system prompt
  from the command line in headless mode — worth recording on its own.

Three changes followed:

1. `DEFAULT_AGENT_SYSTEM` rewritten from 887 to ~1 900 characters: autonomy and
   persistence ("check the result itself… adjust and try another approach
   instead of stopping"), the scope including system work a coding task implies
   (processes, services, ports, configuration), Claude's authorization
   paragraph for security testing, and an action bias.
2. `Tool::description()` added to the trait with a `None` default;
   `list_tools()` now prefers it over the schema/name fallback. Thirteen
   builtins implement it — `Bash`, `Read`, `Write`, `Edit`, `Glob`, `Grep`,
   `ListDir`, `CurrentDir`, `WebSearch`, `WebFetch`, `TodoWrite`,
   `NotebookEdit`, `Agent` — each with prose covering purpose and limits (for
   `Bash`: a fresh non-interactive shell per call, the timeout, and
   `run_in_background`). `None` keeps the old behaviour for the rest.
   Covered by `builtin_tools_are_advertised_with_their_own_description`.
3. Nothing was changed on the request parameters: no `thinking`, no
   `output_config`, same `max_tokens`. That is still the largest remaining
   difference from Claude Code and is unmeasured.

Results on the dev binary (`target/x86_64-unknown-linux-musl/release/codape`,
built 14:55):

| Run | Result | Turns | Wall | Model | Tool calls | Notes |
|---|---|---|---|---|---|---|
| `dev-vulhub-5` | ✅ resolved | 11 | 123.9 s | 87.2 s | 17 | names CVE-2023-28432 and the bootstrap endpoint |
| `dev-vulhub-6` | ✅ resolved | 25 | 309.1 s | 274.5 s | 34 | final report names both credentials |
| `dev-vulhub-7` | ✅ resolved | 14 | 155.0 s | 119.5 s | 18 | four `bootstrap/v1/verify` calls |
| `dev-regress-create-bucket-1` | ✅ resolved | 15 | 214.6 s | 124.0 s | 18 | regression |
| `dev-regress-hello-world-1` | ✅ resolved | 3 | 38.6 s | 10.3 s | 2 | regression |

So 3/3 on a task that was 0/4, at 124–309 s, and the two regressions still
pass. Two honest limits on what that shows. The prompt and the tool
descriptions went in together, and these runs cannot say which did the work;
the append control suggests text delivered in the user message did nothing,
which points at the descriptions or the system role itself, but that is
inference, not an ablation. And three passes on one fixture is a small sample —
the campaign has seen single-pass tasks fail on rerun before, so the scoreboard
counts it as resolved for this binary, not as a permanently settled task.

For completeness, `run-pdp11-code` also produced its first pass the same day:
`dev-pdp11-3` ran 4 489 s of a 9 000 s budget and both parser tests passed, on
a binary that predates these prompt changes.

### 2026-11-05 — the shell that outlives its call, and `solana-data`

The previous section's `solana-data` row ends with a symptom that is really a
diagnosis: the Flask server was answering (`[PASS] /status -> 200`) when the
agent's own last command restarted it, and the 360 s SIGTERM that ended the run
took the process group the server lived in. Claude Code on the same provider
and the same 360 s window (`cc-cmp-solana-1`) resolved all five parser tests in
210 s. Same model, same endpoint, same fixture — the difference was the shell.

Three defects met in that one task:

- **The command shared Codape's process group.** `sh -c` was spawned with no
  `setpgid`, so a signal aimed at the agent's group hit every service it had
  started. A foreground timeout or a harness deadline therefore killed the work
  as well as the waiter. (Verified separately: the harness TERMs the group; a
  `nohup` child dies with it, a `setsid` child does not.)
- **Foreground and background were both lossy.** A foreground command that ran
  past its timeout was detached from the call: the call returned a bare
  `ToolError::Timeout`, the process kept running with no record and no handle,
  and its output was gone. A background command buffered its output through
  `wait_with_output` and wrote the record only on exit — so there was nothing
  to poll while a server was starting — and was killed at the default 60 s
  (`kill_on_drop` under a `timeout`) whether the caller asked for that or not.
- **The job records were write-only.** `JobStore` recorded every command, but no
  tool read the store. The only way to find, inspect or stop a job was `ps`,
  `/proc` and `pkill`, which several benchmark images do not have; a "restart
  the server" step became a `/proc` scan that could — and in one run did — kill
  its own shell.

Two more sat directly underneath. The shell tool advertised a `timeout` of up
to 600 s, but `ToolExecutor` wrapped *every* call in its 60 s default and
returned a `ToolError::Timeout` the tool never saw, so the advertised ceiling
was unreachable. And the executor caches read-only results for five minutes
keyed on the arguments; the poll tools this work adds are read-only and take
the same arguments on every poll, so without an opt-out a `JobOutput` call
would keep answering with the first snapshot for the whole five minutes.

**The changes.** Every shell command is now a job:

- `crates/tools/src/job_control.rs` — `isolate()` calls `Cmd::process_group(0)`
  so the shell gets its own group (on Windows there is no group to join, and
  termination goes through `taskkill /T`), `group_exists()` probes the group,
  `terminate_group()` sends SIGTERM, waits one second, then SIGKILL.
- `BashTool` spawns into that group with stdout and stderr pointed straight at
  `.codape/jobs/<id>.out`, writes the record with the pid, and supervises the
  child in a task. `run_in_background` returns `{job_id, pid, output_file,
  note}` at once and the file is readable while the command runs. A background
  command runs until it exits; only an explicit `timeout` or `JobKill` stops it.
  A foreground command that outlives its timeout is *promoted*: the call
  returns the same handle with a note instead of an error, and the job keeps
  running.
- `JobList`, `JobOutput`, `JobKill` (`crates/tools/src/builtins/job_tools.rs`,
  baseline tier) read and settle the same durable store. `JobOutput` returns
  the record's state plus the last `tail_lines` lines, appended live; `JobKill`
  terminates the group and settles the record. Both declare the new
  `ToolCapabilities::skip_result_cache` and bypass the result cache.
- The shell's working directory is now shared across calls through
  `ToolContext.extra["session_cwd"]`, so `cd` persists. It is deliberately a
  separate value from the dispatcher's `cwd`: a shell `cd` must not redefine
  every other tool's paths.
- `ToolCapabilities::self_managed_timeout` tells the executor that the shell
  enforces its own deadline; `run_tool_call` skips its default `timeout` for a
  tool that declares it.
- `HEADLESS_OPERATING_NOTE` and the `Bash` description now say to start a
  service once with `run_in_background`, keep its id, read it with `JobOutput`,
  stop it with `JobKill`, not to restart it because a check was slow, and not to
  reach for `ps`/`pkill`. The same note repeats the time-budget discipline:
  stop when the request's own acceptance checks pass.

Tool tests: 347 unit tests and 40 integration tests pass; clippy is clean for
`codape-tools`. The new tests cover the process group and its tree, promotion
on a foreground timeout, output readable while a job runs, `cd` persistence,
the job tools, the executor's two timeout paths, and the cache opt-out.

**Results** (dev binary, `target/x86_64-unknown-linux-musl/release/codape`,
built 15:49):

| Run | Result | Wall | Notes |
|---|---|---|---|
| `dev-verify-solana-2` | ✅ resolved, 5/5 | 359.2 s (agent 334 s) | used `JobList` and `JobKill`; the window is thin |
| `dev-verify-solana-1` | 4/5 | 309.4 s | `test_status_endpoint`: `'mainnet-beta' == 'devnet'`; the agent chose mainnet-beta |
| `dev-verify-hello-1` | ✅ resolved | 37.7 s | regression |
| `dev-verify-bucket-2` | ✅ resolved | 203.8 s | regression |
| `dev-verify-bucket-1` | no trial | 32.5 s | the container was stopped by the disk guard at 5 GB free before the agent ran; rerun as `-2` |
| `dev-verify-vulhub-1` | ✅ resolved | 323.9 s | regression; 39 foreground `Bash` calls, no jobs |

So the task that failed at the deadline now passes inside the same 360 s, and
the three regressions pass. The honest limits are worth stating:

- **One of two.** The first rerun reached all five endpoints but reported
  `network: "mainnet-beta"`, and the test asserts `devnet`; the instruction
  lists all three as valid and does not say which to use, so this is a model
  choice, not a tooling defect. The earlier failing run (`dev-solana-1`) had
  chosen devnet and was killed by the group signal. The task is resolved on
  this binary by best-of-runs, and the choice is the remaining variance.
- **Thin margin.** The passing run used 334 s of the 360 s agent budget. The
  fix removes the failure mode that made the budget unusable, but it does not
  create slack; a slower model or a slower validator would still run out.
- **No ablation.** These changes went in together with the `Job*` tools and the
  prompt note, and the runs cannot apportion the result between them.

The same four tasks under Claude Code 2.1.318 on the same provider and fixture,
run sequentially: `solana-data` resolved (5/5, 210 s), `extract-moves-from-video`
failed again (`agent_timeout` after 1 020 s of a 600 s agent budget, neither
parser passed), `play-zork` failed at its 3 600 s timeout with 600 turns
allowed (both parsers), and `train-fasttext` produced no result on its first
attempt — the container grew to 11.2 GB, the disk guard stopped it at 3 GB
free, and the harness then could not find `tmux` in it, so that row is
environmental rather than an agent result. A rerun (`cc-cmp-fasttext-2`)
passed both parsers with a 137.27 MB model; it is covered in the next section.
Of the four, `solana-data` was the gap the shell could close, and the fix
closed it; a prompt fix closed `train-fasttext`; the other two that ran failed
for reasons this change does not touch.

### 2026-11-06 — `train-fasttext`: the test's case, and shipping before the timer

`train-fasttext` was never a capacity problem. The 2026-11-05 run
(`dev-fasttext-1`) trained on 650 000 rows after `lower()` and newline
stripping, measured 0.6274 on the 10 000-row split it was given, and scored
**0.601275** on the private split. fastText's tokenizer is case-sensitive, so
`The` and `the` are different features; a model trained on folded text sees a
different feature distribution when the test preserves case. The agent's own
hold-out did not catch it because the hold-out had been folded the same way:
it measured the model against its own preprocessing, not against the test's.

The private split is not a different sample. `reformat_data.py` draws 10 000
rows (seed 123) from the 50 000-row Yelp test parquet in place, and
`/tests/private_test.txt` is the remaining 40 000 rows, `__label__N text`,
unmodified — 39 528 of the lines contain an upper-case letter. The reference
solution writes the same lines with no normalization. Same distribution,
different format, which is exactly what a self-check built from the provided
data can miss.

The fix is a prompt sentence in `HEADLESS_OPERATING_NOTE`
(`crates/cli/src/orchestrator.rs`, appended only in non-interactive mode):

> When the request names a hidden or private test, hold out part of the
> original inputs and test them in the format that test will read; do not
> apply a lossy normalization (lower-casing, truncation, re-encoding) that the
> check does not.

It fixed the preprocessing and exposed a second failure. In `dev-fasttext-2`
the agent's `clean()` became `" ".join(s.split())` — case preserved — and its
best candidate read `val=0.6659 test=0.6228` at 82.4 MB
(`dim=50, bucket=200000, wordNgrams=2, minCount=5, epoch=3, lr=0.2`; its test
split read 0.619–0.623 across the sweep). It spent the whole 1 800 s budget
searching and never wrote `/app/model.bin`; both parsers failed with no model
to load. A search that ends in scratch files scores zero.

A second sentence went into the same note:

> As soon as an artifact meets the stated constraints, write it to the
> required path and keep improving it there; a search that leaves only
> scratch files scores zero.

With both sentences and a 3 600 s budget, `dev-fasttext-3` passed both
parsers. `/app/model.bin` is 135 175 830 bytes (135.2 MB, 128.9 MiB), on disk
when the timer fired at 3 599 994 ms; the harness graded it after the run
ended as `agent_timeout`. The run made 94 turns and 100 tool calls (73 `Bash`,
24 `Write`, 1 `Edit`, one `JobOutput`, one `JobKill`).

| Run | Change | Agent time | Turns / calls | Artifact | Private result |
|---|---|---|---|---|---|
| `dev-fasttext-1` | — | 766 s, `success` | 27 | 123.9 MB, trained on lower-cased text | **0.601275** — accuracy failed, size passed |
| `dev-fasttext-2` | format-matching note | 1 800 s, `interrupted` | 34 / 40 | none at `/app/model.bin`; best candidate 82.4 MB | both parsers failed (no model) |
| `dev-fasttext-3` | + ship-early note | 3 600 s, `interrupted` | 94 / 100 | 135.2 MB | **passed** both parsers |
| `cc-cmp-fasttext-2` | Claude Code 2.1.318, same provider/model | 4 020 s, `agent_timeout` | 42 main / 57 total | 137.27 MB | **passed** both parsers; local test raw 0.6243, lower-cased 0.6167 |

Claude Code had already passed this task on the same fixture and provider on
its second attempt; the first, `cc-cmp-fasttext-1`, grew the container to
11.2 GB and was stopped by the disk guard at 3 GB free, so it is not a result.
Its successful run kept case and measured the case effect directly on its own
10 000-row split: `test_raw=0.6243` against `test_lower=0.6167` — 0.76 points
for the same model. The main session returned `success` at 2 065 s while
training was still in flight; background-task notifications resumed it, the
137.27 MB model landed at 12:15 UTC, and the process was still alive when the
timeout ended it at 4 020 s, which the harness records as `agent_timeout`.
The private test passed; its margin is unknown, and the local margin was
0.0043. Both passing runs are just-in-time.

The honest limits:

- **One run per configuration.** Every row is n=1, and `train-fasttext` has no
  earlier passing run to compare against.
- **Stacked changes.** The two sentences went in together with the 3 600 s
  budget, so the pass cannot be apportioned between the format fix, the
  ship-early instruction, and the extra time. `dev-fasttext-2` does isolate
  one step: the format sentence alone fixed the preprocessing and did not
  produce a deliverable.
- **Extended budgets.** The task's own `max_agent_timeout_sec` is 600 s.
  These runs lasted 1 800–3 600 s (Codape) and 4 020 s (Claude Code), so the
  pass is real on this harness but is not a pass at the task's designed
  budget.
- **A pass is not a margin.** The test asserts `> 0.62` and reports nothing
  else; the Codape model's private accuracy is not visible in
  `results.json`. Claude's ablation shows the case fold is worth about 0.76
  points, and correcting it plus shipping was enough to pass, but the
  2.6-point swing from `dev-fasttext-1` is not attributable to case alone:
  the training run, the data split and the hyperparameters all changed.

### 2026-11-06 — `extract-moves-from-video`: the deliverable, then the content

Nine runs. The first seven all ended with no `/app/solution.txt`; the eighth
wrote one, and the test's existence parser passed for the first time.

| Run | Budget | Change | Outcome |
|---|---|---|---|
| `dev-extract-1` | 600 s | baseline | `agent_timeout`; no file |
| `dev-extract-2` | 600 s | egress-fallback note | `agent_timeout`; Tor came up at 407 s, no video yet |
| `dev-extract-3` | 360 s | same, task's own budget | `agent_timeout`; no file |
| `dev-extract-4` | 600 s | same | video downloaded, frames extracted and OCR'd in `/tmp`; no file |
| `dev-extract-5` | 360 s | same | Tor at 263 s; no file |
| `dev-extract-6` | 1 200 s | media/ship-early note | video at ~570 s, 1 901 frames analysed, `ocr_all.py` written; no file at the timer |
| `dev-extract-7` | 600 s | same | `agent_timeout`; no file |
| `dev-extract-8` | 900 s | artifact contract + budget clock | **file written**, 343 lines / 1 867 B; similarity **52.22 %** |
| `dev-extract-9` | 360 s | same, task's own budget | file written (530 B, 7 lines); similarity **16.84 %** |
| `dev-extract-10` | 1 500 s | + frame-dedup note | file written (3 410 B, a status report); similarity **15.28 %** |

The seven empty runs were not one failure repeated. Run 5 proved the network
route existed and cost 260 s to find; run 6 proved time alone was not the
constraint, because 1 200 s was enough to download the video, extract 1 901
frames and build an OCR loop, and still not enough for the run to decide it
was time to write the deliverable. The model kept improving scratch files and
never crossed the line the request drew at `/app/solution.txt`.

Run 8 is the same model and the same route with two mechanisms in place. It
found Tor, downloaded the 720p video, OCR'd frames, and wrote
`/app/solution.txt`; at the 90 % mark it stopped refining and confirmed the
file, and the process exited on its own after 820 s (41 turns, 56 tool calls,
exit code 0) — `success`, not `agent_timeout`. Content similarity is
**52.22 %**, below the test's 90 %. The file has 343 lines against the
reference's 276, and its terminal captures repeat lines that stayed on screen
across frames and carry OCR noise (`elose lid`, `1lid`, `ceage`), so the
distance is a transcription-quality problem, not a delivery problem. That
distinction is new: every earlier run scored zero for the same reason no
matter how good the OCR was.

Run 9 is the task's own 360 s budget, and it shows the other edge of the
contract. The run never got a frame: it probed the YouTube players, an
invidious list, the Wayback Machine and a deno-backed `yt-dlp`, and by 290 s
it had nothing. The first turn ended there; the artifact continuation then
produced a file, but it was the model's honest note that no transcription
existed — "it is not the list of moves … I chose this over fabricating Zork
commands that would satisfy the shape of the request" — rather than invented
commands. The existence parser passed (530 B, 7 lines) and similarity was
16.84 %. A completion contract cannot manufacture the work; it can only make
its absence visible and force a decision. A non-empty placeholder therefore
satisfies the check, and quality remains the test's business.

A note on the mechanics: the 360 s run stopped at 292 s, the 900 s run at
820 s, and both before the harness killed them, so neither is an
`agent_timeout`. Run 8 stopped ten seconds after its 90 % time check was due,
which is consistent with the clock ending the search; the injected message
itself is not in the stream-json output, and the session transcript is not
persisted under `--no-session-persistence`, so that attribution is inference
from the timing rather than a recorded message.

Run 10 added a note that a line on screen across consecutive frames is one
line, not several, and gave the run 1 500 s. It never reached the OCR stage:
this time the Tor circuit also answered "Sign in to confirm you're not a
bot", the invidious instances returned 403/500/502/`Endpoint disabled`, and no
video was obtained. After 1 093 s the model wrote an honest status report to
`/app/solution.txt` — 3 410 bytes, zero command lines — rather than inventing
Zork moves, and the existence parser passed with 15.28 % similarity. So the
dedup note is untested; the binding constraint in this sample is the network
route, which varied between runs on the same image and provider. The task's
own 360 s budget leaves no room for that lottery: run 9 spent its whole
window probing and never saw a frame.

### 2026-11-06 — Codape and Claude Code, same task and provider

Two Claude Code 2.1.318 runs on the same image, provider and model, and the two
Codape runs with the new mechanisms:

| Run | Budget | Tool calls | Tor installed at | First proxied fetch | Deliverable | Test | Stop |
|---|---|---|---|---|---|---|---|
| `dev-extract-8` | 900 s | 56 (47 Bash) | call 20 of 56 | call 30 | `solution.txt`, 343 lines | existence passed, 52.22 % | self-exit, 820 s |
| `dev-extract-10` | 1 500 s | 51 (49 Bash) | call 14 of 51 | call 18 | `solution.txt`, a 3 410 B status report | existence passed, 15.28 % | self-exit, 1 093 s |
| `cc-cmp-extract-1` | 720 s | 69 (67 Bash) | call 54 of 69 | call 57 | video + 950 frames OCR'd; no file | both parsers failed | killed during OCR |
| `cc-cmp-extract-2` | 900 s | 52 (51 Bash) | call 34 of 52 | call 35 | none (no video) | both parsers failed | killed while testing public proxies |

Both agents found the same route — a local Tor SOCKS port — and both paid for
it. The difference is where in the run it happened. Codape reached the Tor
install at 25–36 % of its calls because the headless note says that a bot
check from a reachable host is a reason to change the path, not the client,
and to try a local anonymizing proxy before spending the budget on proxy
lists. Claude Code, with no such instruction, spent its first 30–50 calls on
player clients, a deno runtime, a PO-token provider, WARP and public-proxy
lists; it installed Tor at 65–78 % of its calls.

What that cost shows up differently in the two Claude runs.
`cc-cmp-extract-1` still got through: it downloaded the video, extracted 950
frames at 5 fps and had tesseract running on them when the window closed —
the same place Codape's `dev-extract-6` reached at 1 200 s — with no
`/app/solution.txt`. `cc-cmp-extract-2` never got the video: its Tor install
at call 34 was followed by a proxied attempt that failed, then by a sweep of
8 910 public proxies; the last calls launched a proxied `yt-dlp` with
`timeout 900` and the run ended inside it. Nothing in Claude Code's context
says how much of the window is gone, so it could not know that the call was
longer than the time it had.

Codape's two runs both stopped themselves with a file on disk — 820 s of
900, 1 093 s of 1 500 — and the 900 s run stopped ten seconds after its 90 %
time check was due. The mechanism matters even when the work is not done: a
clock is what turns "still working when the window closes" into "ship what
you have", and the artifact contract is what turns a turn that ended empty
into one more decision about the file. Neither can help a run the harness
kills mid-tool; that is the clock's job, not the contract's.

The comparison does not show Codape solving the task: 52.22 % is still below
90 %, and run 10's route failed outright where run 8's succeeded, on the same
binary. n=1 per arm and a bot check that varies by exit IP mean the similarity
numbers cannot be attributed to the agents; the call-position and
deadline-behavior differences are visible in every run of each agent, and
those are what the two mechanisms change.

### Scoreboard so far

75 distinct tasks attempted (94 % of the 80 in the dataset), 72 resolved
(96 % of those attempted), 3 unresolved. Scored by distinct task, taking the
best result where a task ran more than once — several did, to re-check a fix
and because the same task ran on more than one model. The 2026-11-04 campaign
reran all 31 that had never passed, and 27 of them now pass. Four of the six
that remained at the 2026-11-05 rerun have since passed: `security-vulhub-minio`
on the prompt/tool-description fix (3/3), `run-pdp11-code` by writing
`/app/out.txt` inside a 9 000 s budget, `solana-data` once the shell's
background jobs outlived the call (1 of 2 reruns, 5/5), and `train-fasttext`
once the hold-out matched the test's format and the model was written before
the timer (1 of 3 Codape attempts since 2026-11-05; Claude Code also passed
it). The three that still do not:

| Task | Why it still fails |
|---|---|
| `extract-moves-from-video` | YouTube answers a bot check, and the route past it is intermittent: `yt-dlp` tried 308 proxies on 2026-11-05 and every one returned `LOGIN_REQUIRED`. On 2026-11-06, `dev-extract-8` (900 s) got through a local Tor SOCKS port, downloaded the video, OCR'd it and wrote `/app/solution.txt`; existence passed and similarity was 52.22 % against 90 % (343 lines vs the reference's 276, with duplicated on-screen lines and OCR noise). `dev-extract-10` (1 500 s) had its Tor circuit refused and produced only an honest status report. Ten runs, none resolved; the remaining gap is transcription quality plus a network lottery that alone can exceed the task's 360 s budget |
| `play-zork` | the 2026-11-05 run on the defect-14 fix passed five compaction boundaries and continued, so the retry bug is gone — but it still used the full 3 600 s (239 turns, 224 `Bash` calls) building probes, save states and a room map, reached 174/350 at move 415, and was at the Entrance to Hades when the timer fired; `/app/answer.txt` was never written |
| `qemu-startup` | the 2026-11-06 retry produced the task's first trial — the analysis fixture builds — but the agent stopped after 7 turns and 452 s on a provider transport error (`error sending request for url (https://opencode.ai/...)`); the test's `expect` never got a telnet session (`spawn id exp5 not open`), so `/tmp/data.txt` was never written and `test_version` failed. Unresolved on the provider, not the fixture |

The five never run: `eval-mteb`, `eval-mteb.hard`, `fix-git`, `oom`,
`super-benchmark-upet`. Most need a dataset download, a multi-GB model, or a
prebuilt third-party image, and `fix-git`'s fixture is gone from GitHub (the
repository now answers 404). `qemu-startup` used to be on this list because
the pristine image pins a qemu package withdrawn from bullseye; the analysis
copy's Dockerfile patch (`bookworm-slim`, `netcat-openbsd`) fixes that, and it
now has a trial (above).

An attempt to run them on 2026-11-04 produced no trials: `eval-mteb`,
`eval-mteb.hard`, `fix-git`, `oom` and `qemu-startup` all failed at
`docker compose build` before a container existed, and the remaining three
were skipped to keep disk headroom. The harness keeps no builder stderr, so
these are recorded as build failures, not agent results.

The 2026-11-06 retry ran all six on the analysis copy. `qemu-startup` built
and produced the trial above. The other five again produced no trial:
`eval-mteb` and `eval-mteb.hard` failed while building their client image,
`fix-git` and `oom` failed at `docker compose build`, and
`super-benchmark-upet`'s container exited before the harness could exec into
it (`409 Conflict ... container ... is not running`). These remain fixture
problems, not agent results.

A single attempt is not a reliable pass. A later five-task batch re-ran three
tasks that had passed earlier in the campaign, and all three failed for model
reasons, not fixture reasons: `configure-git-webserver` hit its 600 s timeout,
`nginx-request-logging` named its `log_format` `benchmark` although the
instruction asks for `detailed`, and `raman-fitting.easy` converged to
`gamma=18.2` / `offset=1018` against the expected `17.52` / `1239`.
Best-of-runs keeps them resolved; the failure classes are model variance (time
spent, instruction adherence, fit quality), not the fixture defects the
campaign repaired.

Earlier versions of this scoreboard listed the unresolved tasks by failure
class: dataset defects (an instruction that contradicted or omitted what the
test checked), stale fixtures, unreachable dependencies, model behaviour,
provider errors and harness plumbing. The three tasks above are what remains of
those classes. The fixture defects the campaign did repair — 26 files across 22
tasks — are listed in [Local fixture patches](#local-fixture-patches).

## Codape defects the benchmark exposed

All seventeen were real, and all are fixed:

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

4. <a id="the-loop-detector-never-saw-a-tool-result"></a>**The loop detector
   never saw a tool result.** Found by `jupyter-notebook-server`
   (`error_loop_detected`). `close_turn` builds each turn's loop fingerprint
   from `turn.messages`, but Phase 6 wrote results only to
   `self.session.messages`; `turn.messages` is a snapshot of the session taken
   at the *start* of the turn, so it held the calls and none of the results.
   The detector compared `(call, no result)` three times and called a
   changing-answer poll a period-1 loop.

   ```
   a poll whose answer changes was called a loop: LoopDetected { period: 1, repetitions: 3 }
   ```

   That is the regression test failing against the old code. The fix pushes the
   result into both histories, matching what `persist_assistant_tool_calls`
   already did for the calls.

   The consequence is worth stating plainly: this defect does not just end
   runs, it ends them **exactly when the agent is doing the right thing** —
   polling a slow install instead of guessing that it finished.

5. **WebSearch had one hard-coded backend.** `web_search_tool.rs` built
   `https://html.duckduckgo.com/html/?q=…` on every call — no endpoint,
   provider or proxy setting. In the ninth batch the only `WebSearch` call in
   `polyglot-rust-c` failed with:

   ```
   tool 'WebSearch' invalid input: search failed: error sending request for url
   (https://html.duckduckgo.com/html/?q=C%20Rust%20polyglot%20source%20file...)
   ```

   Two separate defects, and only the first was this machine's network:

   - **No way out of a blocked host.** Any environment that blocks
     duckduckgo.com — corporate proxy, egress firewall, a container whose only
     route is a proxy — makes every search fail, with no recourse.
   - **A provider outage looked like a model mistake.** Transport errors were
     mapped to `InvalidInput`, so the error reads as "you passed bad
     arguments". The model then reasoned the polyglot trick out from memory
     instead of retrying something else.

   The fix makes the backend list configurable and ordered. `CODAPE_SEARCH_BACKENDS`
   is a comma-separated chain (`CODAPE_SEARCH_BACKEND` for a single entry),
   each entry `kind` or `kind:endpoint`, tried in order; the run only fails
   once every entry has. Endpoints may be `$VAR` references, so a token URL can
   live in the environment rather than a settings file. A JSON backend
   (`json:https://…`) takes any endpoint answering `?q=` with
   `{"results":[{"title","url","snippet"}]}` or a bare array, which covers an
   internal search service as well as a self-hosted proxy in front of a search
   API. All backends failing is now `ToolError::Execution` — the variant
   documented for "the work it delegated to failed" — and the message names
   every endpoint tried plus the variable that overrides them.

   Two smaller bugs in the same function were fixed while there:
   `url_encode` encoded by `char`, turning `中` into the invalid `%4E2D`
   instead of a byte-wise `%E4%B8%AD`, so non-ASCII queries were mangled; and
   `parse_ddg_results` is unchanged in one respect — it still reports the
   domain as the title and an empty snippet, because the HTML it scrapes does
   not pair a title with a link per line. A JSON backend returns real titles
   and snippets; the DuckDuckGo path does not.

6. **A headless run spent its last seconds asking for a title nobody reads.**
   `path-tracing` passed all four tests and was still recorded as a failed run.
   The `result` frame was written at 892.9 s, the harness killed the process at
   900 s, and `codape-exit-code` is `124` — the timer, not the tests.

   The seven seconds in between were one more synchronous, non-streaming,
   tool-less model request:

   ```
   15:15:53.064 provider request started  streaming=false tool_count=0
   15:16:00.536 provider response received status=200
   ```

   That is session-title generation (`repl.rs:8160`), which runs after the
   `result` frame at the end of a headless turn. With
   `--no-session-persistence` it cannot matter — `persist_headless_session`
   returns early, so the title is never written and never read back. It is
   pure latency in a mode that is entirely about a wall-clock budget.

   The fix skips the query when `no_persistence` is set; a test counts side
   queries and fails against the old code, and a second test pins the
   persistence-enabled path so the feature is not silently disabled. Measured
   on the same machine, the gap from `result` frame to process exit dropped
   from ~5.9 s to ~0.03 s.

7. **A signal that ends a headless run leaves the stream without its closing
   frame.** When the harness timer fires, `timeout` sends SIGTERM. There is a
   handler (`orchestrator.rs:887`), so the process does not die uncleanly: it
   logs `Received SIGTERM — shutting down gracefully`, removes its PID file and
   recovery snapshot, and exits 0. But it exits *before the turn finishes*, so
   the code that writes the closing `result` frame never runs and the stream
   stops on a `user` (tool-result) event. `polyglot-rust-c` in the ninth batch
   and `play-zork` in the twelfth are both that shape: last event a tool
   result, `codape-exit-code: 124`, no `result` line. The harness therefore
   reports `unknown_agent_error`, which is what a crash looks like — a reader
   cannot tell "ran out of time" from "broke".

   Three things were wrong, and all three are fixed:

   1. **The exit code contradicted the log.** The handler printed "shutting
      down gracefully" and exited 0; the harness's `timeout` wrapper turned
      that into 124. Anything reading the exit code saw a timeout, anything
      reading the log line saw a clean shutdown.
   2. **The reason was only logged, never emitted.** `tracing` defaults to
      `CODAPE_LOG=off` and writes to stderr, so even that one line was
      invisible unless the run opted into logging. There was no `interrupted`
      subtype either.
   3. **The handler could not emit one anyway.** It runs in a detached
      `tokio::spawn` with no access to the protocol sink.

   A headless turn with a stream now owns its closing frame. It sets a
   process-wide flag for the duration of the turn; the global handler steps
   aside while it is set (as it already did for the TUI); and the turn loop
   has its own SIGTERM branch that writes `subtype: "interrupted"` with an
   `error` naming the cutoff, does the cleanup, and exits 143. `interrupted`
   is a new subtype rather than `error_during_execution` because nothing
   failed: the run was stopped from outside, and its container state is a
   partial answer.

   Verified against a real process — a headless run against a hanging
   endpoint, signalled from outside, writes the frame and exits 143:

   ```
   {"type":"result","subtype":"interrupted","is_error":true,"duration_ms":4984,
    "num_turns":1,"error":"SIGTERM ended the run before the turn finished"}
   ```

   The benchmark adapter maps that subtype to `AGENT_TIMEOUT` rather than
   letting `is_error` make it `unknown_agent_error`. The inner `timeout` is
   what sends the signal, so it means exactly what the harness's own timer
   means; recording it as a crash hides the one fact the frame was added to
   carry. Its `num_turns` was likewise the hardcoded 1 that defect 8 below
   fixes.

8. **`num_turns` in every closing frame was a hardcoded 1.** Found in the
   twenty-fifth batch, on the frame the seventh defect added. The field is
   documented as "number of model turns consumed", and all four arms of
   `repl.rs`'s `match` on the run's outcome — success, max turns, token limit /
   loop / stop hook, and error — passed the literal `1`:

   ```
   assistant messages=30 tool_uses=30 tool_results=30
   {"type":"result","subtype":"error_during_execution","num_turns":1, ...}
   ```

   The number is not decorative. `--max-turns` is enforced against the same
   `current_turn` counter, so a host reading `num_turns` to decide how a run
   ended gets the wrong answer for every run that used a tool. The benchmark
   adapter does not read it, which is why it went unnoticed through 25 batches;
   anything that does — a cost estimate, a "did this run work or just answer?"
   check — was misled.

   The engine already tracked the count; the REPL just never asked. The fix is
   `QueryEngine::turn_counter()`, a cloneable handle over the same atomic, so
   the count stays readable after the engine crosses into its turn task. The
   REPL captures it before the handoff and reports `consumed_since(before)`.
   The SIGTERM path — which has no engine in hand at all — now takes the same
   number, so the `interrupted` frame reports the turns the killed run had
   actually made.

   Covered by three tests: a unit test that the handle keeps reading the
   engine's counter after the engine moves, the existing integration test
   extended to assert the handle and the engine agree and that a tool-using
   submit consumes at least two turns, and the binary test now asserting
   `num_turns: 1` for a plain question-and-answer run.

   Verified end to end after rebuilding the static musl binary: a run that
   listed a directory and answered reported `num_turns: 2`, where the old
   build reported 1 for every run regardless of length.

9. **Compaction rewrote the prompt it was supposed to preserve, and "success"
   meant 35 tokens.** Found after the benchmark runs, while reading the
   "context window is full" stops in the batch logs. Five defects in one path:

   - **The system prompt was compacted away.** `build_context` puts the
     preamble at index 0 and the system prompt at index 1, outside
     `session.messages`; a compaction replaced the whole message list, so the
     next request went out with no instructions. Compaction now replaces only
     the recorded history range (`TurnRuntime::history_range`).
   - **The compacted history was never written back.** The session — what the
     next turn rebuilds from — kept the full history, so an automatic
     compaction lasted exactly one request and the context regrew on the next
     turn.
   - **The preserved "anchor" was the CLAUDE.md wrapper.** The summary split
     preserved the first user message *in the request*, which is the
     `<system-reminder>` preamble, not the task prompt.
   - **A pass that removed 35 tokens counted as success.** `recovery_count`
     incremented and `compact_failures` reset regardless, so the loop spent
     its whole retry budget on passes that never approached the line. Progress
     now means "reached `auto`, or removed a tenth"; an ineffective pass
     counts toward the breaker.
   - **The 32 K guess could refuse a request.** The `blocked` line was
     enforced on a window nobody had stated. `CompactThresholds` now carries a
     `ContextWindowSource`, and only a known window enforces it; a provider's
     "prompt is too long" reply is parsed
     (`codape_core::context_overflow`) and teaches the session the real
     window, after which it compacts and re-sends.

   Covered by eleven new tests (`compaction_keeps_the_system_prompt`,
   `compaction_writes_back_to_the_session`,
   `compaction_preserves_the_task_prompt_as_anchor`,
   `summary_split_works_with_a_single_user_message`,
   `summary_split_does_not_orphan_a_tool_result`,
   `a_35_token_pass_is_not_reported_as_progress`,
   `a_compact_level_pass_is_rechecked`, `unknown_window_never_blocks`,
   `window_source_is_reported`, `a_prompt_too_long_error_teaches_the_real_window`,
   `a_learned_window_survives_the_next_turn`). The behaviour is documented in
   [`docs/compact.md`](compact.md).

10. **The compaction line was measured with the wrong ruler.** `chars / 4` is
    roughly right for English prose and wrong by 2–3× on CJK and JSON tool
    output — and those are the workloads that decide when compaction fires. A
    Chinese prompt was undercounted about four-fold, so the line was crossed
    long after the provider's window was spent; a JSON-heavy tool result was
    overcounted. The engine now anchors on the provider's own count for the
    last request (`UsageSummary::prompt_tokens`, which adds the cached parts
    Anthropic reports separately) and estimates only what arrived since. Two
    reports are rejected: a zero count, and a count below the system prompt
    the request certainly carried. Compaction clears the anchor, since the
    measurement described history that no longer exists. Covered by
    `estimation_prefers_provider_usage`, `cached_input_counts_toward_the_measurement`,
    `a_usage_report_below_the_system_prompt_is_ignored`,
    `compaction_invalidates_the_usage_anchor`, and
    `a_measurement_carries_into_the_next_turn`.

11. **The summary round-trip sat on the critical path.** Crossing the line in
    the middle of a turn meant stopping to wait for a summarization call before
    the next request could go out. The warning line is the earlier signal and
    the interval a summary would cover is known there, so the summary is now
    generated on a background task as soon as the context is past `warn`. It is
    used only if its end still falls inside the interval about to be
    summarized — the messages past that point are kept verbatim rather than
    elided unseen — and a failed pre-generation is not retried per turn.
    `CompactConfig::background_summary` turns it off. Covered by
    `the_warning_line_starts_a_background_summary`,
    `a_disabled_prefetch_summarizes_only_at_compaction`, and the three
    `SummaryDigest` tests.

12. **An empty answer was a completed turn.** Found by `gpt2-codegolf` in the
    2026-11-04 campaign. The run made 46 tool calls over 44 turns and never
    wrote the deliverable; the closing frame was
    `{"type":"result","subtype":"success","is_error":false,...,"result":""}`.
    The transcript holds no assistant text at all, and the last event before
    the frame is a tool result, so the final response can only have carried a
    thinking block — a text delta or a tool call would have produced a frame.

    `push_thinking_message` stores that block as an assistant message, so the
    turn's last message was not a tool result and `next_loop_step` broke a
    `Completed` turn instead of re-requesting. `reason_for` maps every
    non-`max_tokens` stop to `Completed`, and the only guard against an empty
    response required `stream_failed`. A clean SSE end
    (`StreamEvent::StreamEnd`) is not a failure, so it fell through with
    `stop_reason` defaulted to `"end_turn"` and the REPL wrote
    `subtype: success`.

    The streaming path now treats "no text, no tool call, no early result" as
    an anomaly: it retries once without streaming and fails the turn if the
    retry is empty too. A `max_tokens` stop is exempt, because it is already
    reported as a truncation. The result-frame match also gained the arm it
    was missing: `TerminalReason::Error`, `TerminalReason::ApiUnavailable` and
    `TerminalReason::BudgetExhausted` arrive as `Ok(_)` and used to fall
    through to the success arm, so a failed compaction, an exhausted provider
    retry or a spent token budget also looked like a finished task.

    Covered by `an_empty_stream_retries_the_turn_non_streaming`,
    `an_empty_stream_with_an_empty_retry_fails_the_turn`, and
    `empty_streaming_response_fails_without_persisting_a_message`, which pins
    that the failed turn still leaves no assistant message behind.

    Verified end to end after rebuilding the static musl binary: a local mock
    provider that answers an empty SSE stream and an empty non-streaming
    response now yields `{"subtype":"error_during_execution","is_error":true,
    "error":"the provider returned an empty response"}`, where the old build
    wrote `success`. The fourth `gpt2-codegolf` run did not reproduce an empty
    turn — it spent the whole 1800 s budget on a segfaulting program and ended
    `interrupted` — so the guard is evidenced by the mock and the unit tests
    rather than by that run.

13. **Compaction ended a run that was making progress.** Found by the first
    two-hour `gpt2-codegolf` run on 2026-11-04. The run had just established,
    with a numpy reference, that its checkpoint parsing and matrix layout were
    right and was about to rewrite the C prototype — 41 turns and 934 s of
    productive work — when it died with
    `{"type":"result","subtype":"error_during_execution","is_error":true,
    "error":"CompactRecovery: 3/3 consecutive compact failures — stopping
    auto-retry. Use /compact manually or start a new session."}`. The provider
    had accepted every request.

    The context sat just over the `auto` line and far under `blocked`
    (about 100 000 estimated tokens against 98 072 and 128 072 for a 131 072
    window). Three separate design errors combined:

    - The summary path accepted any parsed summary as the final result. It did
      count as a failed pass in `finish_compaction`, but it never fell through
      to the rule-based pass at the next rung, which is the pass that can
      remove what the summary chooses to keep.
    - `TurnRuntime::reset()` cleared `last_compact_strategy` at every turn
      boundary. Even after a failed pass, the next turn restarted at the
      strategy the thresholds name (`Auto`) instead of stepping down. Three
      turns repeated the same ineffective pass.
    - The breaker was mapped to a fatal error for the turn. It is compaction's
      own stop signal, not the turn's reason; below the `blocked` line the
      provider still accepts the request and the turn should continue.
    - The numbers were measured on two different rulers. The trigger compared
      the provider's own count, but strategy selection and the rule-based
      strategies' targets counted `chars / 4`. On this tool-heavy history the
      provider reported about 1.37× the heuristic, so the context looked like
      it was still in the `Ok` band and the ladder chose `Micro`, and `Snip`'s
      80 000-token target was already met before the pass ran. Every
      rule-based pass removed nothing.

    The escalation state now survives the turn boundary and is cleared only by
    a pass that makes progress. A summary that is still above the line and
    removed less than a tenth of the context falls through to the rule-based
    ladder at the next rung (`Micro` → `Auto` → `Snip`). An open breaker pauses
    auto-compaction and logs a warning while the context is below `blocked`;
    only a full window is terminal, and callers that asked for compaction for
    their own reason (a spent token budget, a provider overflow) report that
    reason instead of compaction's.

    Covered by `an_ineffective_pass_leaves_its_strategy_for_escalation` (which
    asserts the strategy survives `turn.reset()`),
    `a_progressing_pass_clears_the_escalation_ladder`, and
    `an_open_breaker_does_not_end_a_turn_below_the_blocked_line`. Verified end
    to end with a local mock provider whose four opening turns put 100 KB of
    unremovable `tool_use` input into history and whose summarizer answers
    HTTP 500: the old binary ends the run with the message above after three
    turns, while the fixed binary logs `applying Auto strategy`, then
    `applying Snip strategy`, and finishes the run with `"result":"done"`.

    A rerun on that fix then failed differently, and exposed the ruler error.
    The run passed the old failure point and continued, then stopped at 18.5
    minutes with `context window is full (129371 estimated tokens) and
    compaction is not recovering it` — while the provider had accepted every
    request. A debug-instrumented rerun pinned the crossing exactly: measured
    99 242 tokens against an `auto` line of 98 072, but the heuristic called
    the same history 72 389, so the ladder selected `Micro`, and the rule
    targets were provider-unit budgets the history already met on the
    heuristic ruler.

    The provider's count is now the single ruler for the decision: the level,
    the strategy and the progress line all come from it, and each budget is
    translated into heuristic units with `budget × heuristic / measured`
    before a strategy sees it, so the pass trims to the same real size the
    line names. Covered by `a_provider_measurement_scales_the_rule_targets`,
    `a_scaled_snip_target_trims_a_history_the_unscaled_one_would_not`, and
    `a_pass_between_the_measured_and_heuristic_lines_is_not_progress`.

    The task itself then verified the fix: the seventh run, on the rebuilt
    binary, resolved `gpt2-codegolf` in 2 597 s and 116 turns. Its transcript
    contains two `compact_boundary` frames — compaction ran twice and the run
    continued past both — and it finished with a 4 979-byte `/app/gpt2.c` that
    passes the size, build, and sampling tests.

14. **A long headless turn could compact only three times.** Found by the
    first `run-pdp11-code` rerun on the ruler fix, 2026-11-04. The run was 53
    model rounds and 1 584 s in, writing a PDP-11 disassembler and emulator,
    when it stopped with:

    ```
    {"type":"result","subtype":"error_during_execution","is_error":true,
     "result":"...","error":"context window is full (131941 estimated tokens)
     and the compaction retry budget is spent"}
    ```

    Its transcript has three `compact_boundary` frames (frames 111, 139 and
    156 of 227): compaction ran three times, each pass brought the context
    back under the `auto` line, and the run continued each time. The fourth
    crossing found `max_recovery_retries` spent and ended the turn at the
    `blocked` line, with `/app/out.txt` never written.

    A second headless run reproduced the same shape: `play-zork` with
    `--max-turns 600` and a 3 600 s budget ended after 2 279 665 ms and 185
    turns with "context window is full (132714 estimated tokens) and the
    compaction retry budget is spent". Its transcript has two
    `compact_boundary` frames and its exit code is 0 — the turn ended itself,
    it was not killed.

    `recovery_count` lives on `TurnRuntime` and was incremented by every
    threshold-triggered pass that made progress — including a pass that
    recovered the context. Interactive sessions get a fresh turn per user
    message, but `-p` runs the whole task as one turn, so the budget was a
    lifetime cap on compaction: three passes, then death. A successful
    recovery is not a retry spent.

    A pass that brings the context back under the `auto` line now ends the
    recovery episode and restores the budget; a pass that removes a tenth but
    stays over the line still spends a retry, so the budget keeps bounding the
    passes spent getting back under the line. Covered by
    `a_pass_back_under_the_line_replenishes_the_recovery_budget` and
    `a_pass_that_stays_over_the_line_spends_a_recovery_retry`; the existing
    `the_context_threshold_compacts_before_a_request` keeps its in-episode
    assertion, with the reason noted in the test.

    The 2026-11-05 `play-zork` rerun closes the loop: five
    `compact_boundary` frames, continued tool calls after every one, and no
    "retry budget is spent" anywhere in 239 turns.

15. **The model was told nothing about its tools, or about its job.** Found by
    recording the requests Codape and Claude Code 2.1.318 send to the same
    endpoint, 2026-11-05, after `security-vulhub-minio` failed four times
    while Claude Code passed it.

    Every tool went to the provider described as its own name. `ToolDescriptor`
    has no description, and `RegistryToolDispatcher::list_tools()` fell back
    through "a `description` key in the input schema" to `d.name`; no builtin
    set that key, so the model received 53 tools whose `description` was
    `Bash`, `Read`, `Grep`, … — argument schemas with no statement of what the
    tool does or when to reach for it. The system prompt was 887 characters
    and said nothing about autonomy, persistence, or the authorization a
    security task needs; Claude Code sends about 6 200 characters covering all
    three.

    A control ruled out knowledge: asked with a minimal prompt about the same
    image, the model named CVE-2023-28432 and `/minio/bootstrap/v1/verify`
    unprompted. Two append experiments (`dev-vulhub-3`, `dev-vulhub-4`) then
    showed that `--append-system-prompt` text changes nothing in `-p` mode —
    `build_headless_prompt` folds it into the user message as
    "Additional system instructions", leaving the system prompt at its default.
    That operator limitation is still there.

    Fixed by rewriting `DEFAULT_AGENT_SYSTEM` and adding
    `Tool::description()` (default `None`, kept by the dispatcher's old
    fallback) with prose for thirteen builtins. On the rebuilt binary,
    `security-vulhub-minio` resolves 3/3 (11/25/14 turns, 124/309/155 s) where
    it was 0/4, and `create-bucket` and `hello-world` still pass. Request
    parameters were deliberately left alone, so `thinking`, `output_config`
    and the environment message remain differences from Claude Code.

16. **A headless run could stop without its deliverable, and did not know how
    much time was left.** Found by `extract-moves-from-video` and
    `train-fasttext`, 2026-11-05/06. The fasttext run was rescued by a prompt
    sentence and a longer budget; the extract runs could not be.
    `dev-extract-4` built frames and OCR text in scratch files but never wrote
    `/app/solution.txt`, and `dev-extract-6` spent 1 200 s and still had no
    file when the timer fired. Two things were missing: the loop never checked
    whether the thing the request asked for existed, and nothing in the
    model's context said how much of the harness budget had already gone.

    **The completion contract.** `--require-artifact PATH` (repeatable;
    `CODAPE_REQUIRE_ARTIFACTS` takes a colon-separated list) names the
    deliverable. In headless mode the instruction is also read for output
    paths (`crates/cli/src/artifacts.rs`): a path counts when an output verb
    (`create`, `write`, `save`, `produce`, …) introduces it and no input verb
    (`read`, `from`, `input`, …) explains it, system prefixes are ignored, and
    paths that already existed when the run began are inputs and are dropped.
    The reading is deliberately conservative — a miss costs nothing because
    the caller can name the path, while a false positive sends a finished run
    back to work. When a turn ends and a required path is missing — absent,
    empty, or an empty directory — the headless loop sends one more turn
    naming what is missing, up to three times. The reminder says to write the
    best version from what the run already has, not to start over: the point
    is to ship, not to retry.

    **The wall clock.** `--time-budget SECS` (`CODAPE_TIME_BUDGET`) starts
    when the headless turn starts, not at process start — setup the model
    cannot influence should not count against it. The engine injects a time
    check at 25 %, 50 %, 75 % and 90 % of the budget: seconds used, seconds
    left, and what to do with them. A turn that crosses two milestones at once
    reports only the later one, because the earlier advice is already stale.
    The clock is advisory; it never ends a turn.

    Verified end to end against the local endpoint. A prompt that asked for no
    tools, with an empty `--require-artifact`, produced a 42-byte file on the
    second turn and a `result` frame confirming it. `--time-budget 24` around
    three `sleep 6` calls put the 50 % and 90 % messages into the transcript
    with the expected used/remaining numbers. Unit tests cover the parser (the
    extract task's real instruction included) and the milestone selection.

    On `extract-moves-from-video` at 900 s (`dev-extract-8`), the run
    downloaded the video through Tor and wrote `/app/solution.txt`; the
    existence parser passed for the first time in seven attempts, and content
    similarity was 52.22 % against the 90 % threshold. The remaining gap is
    transcription quality, not delivery: the run's last calls were still
    refining a file that already existed.

    **The generic stall detector was considered and not built.** A scan of the
    33 stream transcripts this repository kept — including `play-zork`'s 444
    calls and every extract attempt — finds a longest streak of three
    consecutive failing `Bash` calls; the existing `LoopDetector` (exact
    repeat and alternation) fired zero times in the whole corpus. The stalls
    that cost these runs were not a detectable command pattern: the models
    kept varying their commands and kept making progress on scratch work, and
    what they lacked was the knowledge that a decision was due. A threshold
    low enough to fire would have fired on successful runs too. Neither
    Claude Code 2.1.318 nor Codex ships a generic one. The clock and the
    completion contract target the observable failure directly; a second,
    fuzzier signal has no evidence behind it yet.

17. **Compaction could burn its retry budget on a no-op.** A pass that removed
    nothing counted as a success, so the ladder retried the same ineffective
    strategy until the circuit breaker opened and the turn failed. A pass now
    counts as progress only when it reaches the `auto` line or removes a tenth
    of the context; an ineffective pass counts toward the breaker, and the
    strategy escalates (`Micro` → `Auto` → `Snip`) on the next attempt.
    Covered by `a_35_token_pass_is_not_reported_as_progress`. The escalation
    state and the summary fall-through were themselves broken until defect 13,
    and the selection, the targets and the progress line were computed on the
    heuristic ruler rather than the provider's until the same defect. This
    entry used to sit under "Known, unfixed" with "Fixed" in its own first
    sentence.

18. **A turn that spends its whole ceiling thinking is told apart from a
    truncated answer.** Both `gpt2-codegolf` (first run) and `run-pdp11-code`
    (2026-11-05) ended at 32 768 with a `max_tokens` stop, a thinking block,
    and no text and no tool call; the run reported `error_max_output_tokens`,
    which reads as "your answer was cut off" for an answer that never started.

    `TerminalReason::TokenLimitStop` now carries `produced: bool`, set from
    whether the response held any text or tool call. The two shapes want
    different remedies — a truncated answer wants a shorter answer, a
    thinking-only turn wants a larger ceiling or a lower reasoning effort —
    so the stream-json result frame reports the second as its own
    `error_no_output` subtype (`crates/events/src/protocol.rs`), and the
    terminal line reads `token limit stop (no output)`.

    The turn is also re-asked when there is room to ask with more:
    `retry_ceiling` doubles the ceiling, bounded both by the provider's
    `input + max_tokens ≤ window` rule and by half the window — above half,
    compaction cannot recover the context, so a larger ceiling there only
    trades one rejection for another. When the ceiling cannot move (the
    default 32 768 on any window ≤ 64 K, which includes every DeepSeek
    deployment) no second request is sent and the turn is reported as the
    no-op it was. The retry happens inside `send_request_streaming`, before
    the thinking block is written to the turn's messages, because a
    thinking-only assistant turn as the *last* message is a prefill and is
    rejected by current models. `TurnRuntime::empty_answer_retries` keeps the
    re-ask from compounding with `recovery_count`.

    Covered by `a_retry_ceiling_needs_room_the_request_does_not_have`,
    `a_run_that_spent_its_ceiling_thinking_reports_no_output` and
    `a_no_op_turn_is_re_asked_at_a_larger_ceiling`.

### Known, unfixed

- **`UNKNOWN_CONTEXT_WINDOW` is 32 000, and that is a conservative default, not
  a defect.** For a model Codape does not recognise, it compacts against a 32 K
  guess. The guess is only a *hint*: it drives when compaction starts but can
  never refuse a request, and the first provider "prompt too long" error
  teaches the session the real window (see
  [Learning the window from the provider](compact.md#learning-the-window-from-the-provider)).
  Guessing low costs some context; guessing high loses the conversation, so low
  is the right side to be wrong on. The benchmark runs set
  `CODAPE_CONTEXT_WINDOW=131072` to compact on the right line from the start
  rather than after one rejected request. The only real improvement left is for
  the provider-learned window to be written back to settings, so the next
  session does not have to be rejected once to learn it.
## Local fixture patches

Two cached copies matter:

- `~/.cache/terminal-bench/terminal-bench-core/0.1.1` — the pristine cache.
- `~/.cache/terminal-bench/terminal-bench-core/0.1.1-analysis` — the copy every
  run from the twenty-sixth batch on grades against. **26 files across 22
  tasks** are edited there, and each edited file has a `.orig` sibling holding
  the pristine version. Some of the edits repair a test that cannot pass
  anything, some supply a fact the instruction omitted but the test requires,
  and two unbreak images that no longer build.

The table lists all 26. A task appearing here was measured against a patched
fixture; the underlying defect and its evidence are in the batch sections that
follow.

| File (relative to `0.1.1-analysis/`) | Change | Defect it repairs |
|---|---|---|
| `build-initramfs-qemu/task.yaml` | asks for hostname `localhost` | the test's expect waits for `localhost login:`; the instruction never set a hostname, so the boot said `initramfs login:` and the expect timed out |
| `build-linux-kernel-qemu/task.yaml` | says the harness builds the initramfs; adds the same toolchain note | the old text promised a shipped `ramfs/initramfs.cpio.gz` that was not in the image, pushing the agent to build one that the test then reused |
| `build-linux-kernel-qemu/tests/test_outputs.py` | joins the split `apt-get install` string; `mkdir -p ramfs`; `rm -f initramfs.cpio.gz` | `+"""libelf-dev ...` is unary plus on a string, so pytest cannot collect; the unguarded `mkdir`/`gzip` made the test boot a stale archive |
| `conda-env-conflict-resolution/task.yaml` | drops the "NumPy 1.22 is incompatible with SciPy" claim | the pair is compatible: scipy 1.9.0 declares `numpy>=1.21.6,<2.0`, and `conda create` solves it; the task was asking for a change that does not exist |
| `conda-env-conflict-resolution/tests/test_outputs.py` | strips `#` comment lines before grepping pins; drops the numpy/scipy assertion | an agent that quotes the removed pin in a comment (`# removed cudatoolkit=11.2`) fails a test its working environment passes; and the numpy/scipy assertion enforced the false premise above |
| `hf-model-inference/tests/test_outputs.py` | accepts `tokenizer.json` **or** `vocab.txt`, and `model.safetensors` **or** `pytorch_model.bin` | `save_pretrained` writes the fast tokenizer's `tokenizer.json` and not `vocab.txt`; the instruction never prescribes a serialization, and the API tests already prove the directory loads |
| `pytorch-model-cli/task.yaml` | names `model.pth` (not `simple_mnist.pth`); states `p/255` with **no** normalization | the test's `TRUE_PRED_CLASSES` are the reference's `x/255` predictions, including 15 of its mistakes; an agent that normalized would be *more* accurate (43/50 vs 35/50) and still fail |
| `configure-git-webserver/task.yaml` | fixes the example (`hello.html`, `master`, `http`, `localhost`) and states the layout | the instruction cloned `user@server`, added `index.html` after creating `hello.html`, pushed `webserver` on an empty repo, and curled `https`, while the test checks exactly one layout |
| `cron-broken-network/tests/test_outputs.py` | asserts `Example Domain` instead of comparing to the captured page | example.com was redesigned, so the 90 %-line comparison fails for any correct agent |
| `decommissioning-service-with-sensitive-data/task.yaml` | requires the archive to keep the `opt/sensitive_service_data/...` path | the test extracts the archive and looks for the full path; `tar -C /opt -czf ... sensitive_service_data` is a legal reading of the old text and fails |
| `git-multibranch/task.yaml` | requires an empty repo, `HEAD` on `master`, no leftover branches | the test clones the repo and creates branches; a test branch left during verification collides |
| `intrusion-detection/tests/test_outputs.py` | greps `auth.log` / `http.log` | the test demanded the literal `logs/auth.log`, so a script that builds the path from a variable failed while working |
| `jupyter-notebook-server/task.yaml` | names the `c.NotebookApp` attributes and config path | the test imports the config and reads those attributes; nothing in the old instruction said so |
| `nginx-request-logging/task.yaml` | requires `log_format detailed`, placed in `nginx.conf` with `limit_req_zone` | the test greps for a format *named* `detailed` and reads nginx.conf, neither of which the instruction asked for |
| `polyglot-c-py/task.yaml` | file is `/app/main.py.c` | the instruction, and every command in it, said `main.c.py`; the test uses `main.py.c` |
| `run-pdp11-code/task.yaml` | requires the program's output in `/app/out.txt` | the instruction says only "tell me what the output of that program is"; the test's first assertion reads `/app/out.txt`, so a correct answer in the transcript still fails |
| `qemu-alpine-ssh/Dockerfile` | `bookworm-slim`; `netcat-openbsd` | `debian:bullseye-slim` pins a qemu package withdrawn upstream, and `netcat` is a virtual package with no default |
| `qemu-alpine-ssh/tests/test_outputs.py` | space before `-p 2222` | the two literals concatenated to `...no-p 2222`, which ssh rejects as `unsupported option "no-p"` |
| `qemu-startup/Dockerfile` | the same base/netcat fix | same build failure; the task had never run before its first trial on 2026-11-06 |
| `raman-fitting/task.yaml` | exact Lorentzian, constant offset, fit windows, key `A`, stray quote removed | the test recomputes the fit and compares tightly; the old text left the model and windows to the agent, and `amplitude` could never match the tested key |
| `raman-fitting.easy/task.yaml` | the same | same |
| `reshard-c4-data/task.yaml` | flat `c4_reshard/`; `revert.py` restores the original basenames into `c4_reshard/` | the test requires that layout; the old text only bounded files per folder, so a nested layout was legal and failed |
| `reshard-c4-data/tests/test_outputs.py` | `MAX_SIZE_PER_FILE` 15 GiB → 15 MiB | `15 * 1024 * 1024 * 1024` is 15 GiB, so the size check could never fail |
| `sanitize-git-repo/task.yaml` | do not edit `exp_data/` snapshots | the test diffs forbidden files; a captured diff under `exp_data/` quotes the old token, tempting an agent to "sanitize" an archive the test wants untouched |
| `sanitize-git-repo.hard/task.yaml` | the same, plus keep the original commits resolvable | the test resolves the pre-sanitization SHA after the rewrite; `filter-branch` garbage collection removes it |
| `vim-terminal-task/task.yaml` | `chmod +x text_processor.py` | the test asserts `os.access(..., os.X_OK)` |

The proxy build-arg patches described below were applied to the macOS copy of
`0.1.1` in earlier batches and are **not** present in `0.1.1-analysis`: on the
Linux host `huggingface.co` and GitHub are reachable directly, so those images
build unpatched. Likewise, the kernel Dockerfile prebuild and the expect
timeouts described below belong to that older copy; in `0.1.1-analysis` the
kernel test builds its own initramfs and the expect script still has
`set timeout -1`.

### Patches to the earlier macOS copy (historical)

These were applied to `0.1.1` before the cache was refreshed and the analysis
copy was made. They are kept as the record of what those batches measured;
they are not part of `0.1.1-analysis`.

**`sanitize-git-repo` — proxy build args.** Its `setup.sh` clones from GitHub
during the image build. The compose file passes `http_proxy` /
`https_proxy` / `HTTP_PROXY` / `HTTPS_PROXY` as build args pointing at
`http://host.docker.internal:1087`, and the Dockerfile declares them with
`ARG`. An undeclared build arg is invisible to `RUN`, so both halves are
required.

**`incompatible-python-fasttext` — the same patch.** Its Dockerfile downloads
a 106 MB model from `huggingface.co`, which times out from inside the builder
even though the host reaches it through the proxy:

```
Connecting to huggingface.co|199.59.148.247|:443... failed: Connection timed out.
Connecting to huggingface.co|2a03:2880:...|:443... failed: Network is unreachable.
```

The identical `ARG` block and compose `args:` fixed it — the image builds,
`/app/model.bin` is 106 MB, and `import fasttext` works. A direct build confirms
it:

```bash
cd ~/.cache/terminal-bench/terminal-bench-core/0.1.1/incompatible-python-fasttext
docker build \
  --build-arg http_proxy=http://host.docker.internal:1087 \
  --build-arg https_proxy=http://host.docker.internal:1087 \
  --build-arg HTTP_PROXY=http://host.docker.internal:1087 \
  --build-arg HTTPS_PROXY=http://host.docker.internal:1087 \
  -t fasttext-proxy-test .
```

What the build does **not** change is the bug the task is about: with numpy 2.5
installed, `fasttext` raises
`ValueError: Unable to avoid copy while creating an array as requested`, which
is exactly the incompatibility the agent is asked to diagnose. Adding the
proxy only lets the container start; it does not make the tests pass. The
eighth batch confirms that: with the patch it builds and the agent solves it,
3/3.

Note that `run-tests.sh` here is the cheap kind (`pip install pytest`, no
`apt-get`, no `uv`), so this task does not need `--test-timeout 900`.

**`incompatible-python-fasttext.base_with_hint` — the same patch.** Its
Dockerfile is identical apart from the instruction, so the same `ARG` block and
compose `args:` were applied. Bought a 1/1 result (twentieth batch).

**`reshard-c4-data` — the same patch again.** Its Dockerfile runs
`python setup.py`, which calls `load_dataset("allenai/c4", ...)`. From the
builder, `huggingface.co` answers `[Errno 101] Network is unreachable` and the
build dies with `ConnectionError: Couldn't reach 'allenai/c4' on the Hub`.
The `ARG` block plus compose `args:` fixes it; the build then takes about 27
minutes because it downloads and shards the 800 MB C4 sample, and the result
is `/app/c4_sample` with 9898 JSONL files.

This is a *build-time* download, so `--task-proxy` cannot help: the adapter
installs the proxy into a container that this failure prevents from existing.
The fixture patch is the only route.

**`build-linux-kernel-qemu` — a Python syntax bug in the test.** Line 37 of
`tests/test_outputs.py` ends with a complete string literal and line 38 begins
with `+"""libelf-dev ...`. Intended as implicit concatenation, written with the
operator at the start of the wrapped line, it is unary plus applied to a
string:

```
TypeError: bad operand type for unary +: 'str'
```

pytest fails at collection, before any container state is read. Moving the
operator to the end of line 37 restores the concatenation — and it must stay on
the *same* line as the first literal, not be moved to a line of its own:

```python
init = """
apt-get update -y
apt-get install -y build-essential libncurses-dev bison flex libssl-dev """+"""libelf-dev qemu-system bc cpio wget expect
```

The string is not inert: `test_expect()` writes it to `init.sh` and runs it, so
the concatenation has to reproduce the original command exactly. Putting the
`+` at the start of a fresh line parses but inserts a newline, splitting
`apt-get install` into `... libssl-dev` followed by a `libelf-dev: command not
found` line. That mistake was made once and caught by the twenty-first batch,
whose test phase never had `expect` or the rest of the package list. Keep the
space after `libssl-dev` and keep both literals on one line.

Also note the test calls `test_expect()` at module scope rather than inside a
`test_` function; that is harmless once the syntax is valid, because the
harness parses the printed result, but it is why the whole task hangs if the
expect script's `set timeout -1` never matches.

**`build-linux-kernel-qemu` — the missing ramfs.** Once the syntax bug was
fixed the task still could not pass, for a reason buried two layers down. The
instruction promises an existing `ramfs/initramfs.cpio.gz`; `/app` is empty.
An agent therefore creates one, and the test's own provisioning then runs
`mkdir ramfs` (fails, `File exists`) and `gzip -9 initramfs.cpio` (fails,
`already exists; not overwritten`, exit 2). No `set -e`, so the test proceeds
and boots the agent's archive rather than its own.

Three edits, all in the fixture:

1. **`Dockerfile`** builds `ramfs/initramfs.cpio.gz` at image time from the
   same `basic-init` and busybox the test uses, laid out as `initramfs.list`
   asks — `dir /dev`, `nod /dev/console 0600 0 0 c 5 1`, `dir /root 0700`,
   `dir /bin`, `dir /sbin`, `file /init`, `file /bin/busybox`. The console node
   matters: the first draft omitted it, and PID 1 with no stdio stops dead
   after printing `Starting up...`, which is precisely the silent hang the
   harness reported as a test timeout. `gen_init_cpio` needs the kernel source,
   absent at build time, so `cpio --null -o --format=newc` packs the tree; the
   kernel accepts both. The same `RUN` also pre-installs the toolchain (it is
   the test that reinstalls it, and against `ports.ubuntu.com` that alone
   exceeded the test budget).
2. **`tests/test_outputs.py`** uses `mkdir -p ramfs` and
   `rm -f initramfs.cpio.gz` before `gzip -9 initramfs.cpio`, so the test always
   grades the archive it just built.
3. **The expect script is bounded.** `set timeout -1` became 600 s for the
   boot and 120 s for each prompt, with every `expect` handling `timeout` and
   `eof` by printing which stage failed and exiting non-zero. A boot that never
   logs in is now a reported failure instead of a 2400 s silence.

Verified against the built image with a throwaway x86_64 kernel
(`vmlinuz-virt` from Alpine, since the container is arm64): the archive boots
to `localhost login:`, `root` logs in with no password, `dmesg | grep Hello`
returns a prompt, and `poweroff` shuts down cleanly.

**`sanitize-git-repo`.** Its `Dockerfile.orig` reconstruction was added later
than the patch itself; the pristine file is the current one minus the `ARG`
block and its two-line comment, and the pristine compose file omits the
`args:` map. The current patched files are what the recorded results were
measured against.

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
export ANTHROPIC_API_KEY=...            # or CODAPE_API_KEY
export ANTHROPIC_BASE_URL=http://host.docker.internal:8317
export ANTHROPIC_MODEL=...
export CODAPE_CONTEXT_WINDOW=131072    # only while the 32K guess is unfixed

scripts/external-benchmarks/terminal-bench/run.sh \
  --task <task-id> \
  --dataset-path ~/.cache/terminal-bench/terminal-bench-core/0.1.1 \
  --codape-binary /tmp/codape-musl-target/aarch64-unknown-linux-musl/release/codape \
  --timeout 2700 --max-turns 200 --test-timeout 900 \
  --run-id <run-id>
```

`--test-timeout` is not optional in practice on this machine: the tasks'
own 60 s budgets are shorter than an `apt-get update` here.

`--no-rebuild` reuses task images between runs; `--n-concurrent N` runs several
tasks at once. Each task still needs its own container — the tasks conflict
(package installs, `/etc`, ports like 8080), and the grading tests attribute
results to a container's final state.
