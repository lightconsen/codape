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
| `parse_error` | the harness found no pytest summary, so it never scored the run — usually the test script died during its own bootstrap (`apt-get update`, installing `uv`) |

`parse_error` and `test_timeout` are both "the test never produced a verdict".
Check `panes/post-test.txt`: if its last line is a download progress bar, the
test was cut off while installing its own dependencies, and the result says
nothing about the agent.

A trial with `failure_mode: unset` and `accuracy: 0` is a **model or prompt
result**. A trial with any other mode is a **plumbing result** and says nothing
about the model until the plumbing is fixed.

## Environment on this machine

- macOS on arm64; Docker runs `linux/arm64` task images.
- There is no published aarch64 CodeRex release, so runs use
  `--coderex-binary` with a locally cross-built **static musl** binary. A
  glibc build against `rust:1.99.1` links against glibc 2.39, which the
  `bookworm` task images (2.36) cannot load — `GLIBC_2.39 not found` before
  the agent starts. Build static instead:

  ```bash
  export http_proxy=http://127.0.0.1:1087 https_proxy=http://127.0.0.1:1087
  docker run --rm --platform linux/arm64 \
    -v "$PWD":/src:ro -v /tmp/coderex-musl-target:/target \
    -v /tmp/coderex-cargo-registry:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/target -w /src rust:1.99.1 \
    bash -c 'rustup target add aarch64-unknown-linux-musl \
             && apt-get update -qq && apt-get install -y -qq musl-tools pkg-config libdbus-1-dev \
             && cargo build --release --locked --target aarch64-unknown-linux-musl \
                  -p coderex-cli --bin coderex'
  ```

  The result is `/tmp/coderex-musl-target/aarch64-unknown-linux-musl/release/coderex`.
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
  `PASSTHROUGH_ENV` forwards `ANTHROPIC_*` and `CODEREX_*` into the container.
  Give `CODEREX_CONTEXT_WINDOW` the real window — CodeRex assumes 32 000 for a
  model it does not recognise. A trailing slash on the base URL is harmless:
  `request_url` trims it before appending `/v1/messages`
  (`crates/core/src/api_client.rs:1002`).
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

These two are recorded as **dataset problems**, not model or CodeRex problems.
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

Both failures are **underspecified requirements**, not CodeRex defects — and
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
| `jupyter-notebook-server` | ❌ unresolved | **CodeRex defect** — `error_loop_detected`, now fixed |
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
not a CodeRex result.** Re-run with `--test-timeout 900` to score it.

There is a second, independent problem in this trial: the agent began with
`tmux kill-server 2>/dev/null`, which destroys **every** tmux session on the
machine — including the harness's own, which is how Terminal-Bench sends
keystrokes and captures panes. `panes/post-agent.txt` is the whole failure:

```
no server running on /tmp/tmux-0/default
```

and `coderex-exit-code` is `137` (SIGKILL) rather than a clean exit. The
harness created the `agent` session and, at 05:12:00, the `tests` session; the
agent's `kill-server` took them both out and `tmux ls` afterwards showed only a
`tests` session that had been re-created. This is **model behaviour conflicting
with the harness's transport**, not a CodeRex bug — CodeRex ran the command the
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

The agent's last turn had already fixed the package, `coderex-exit-code` is
`0`, and the test passed. This is a provider-key failure, not a CodeRex failure,
and it is the reason the switch to KIMI was made. Worth knowing when reading
`unknown_agent_error` in these results: check `coderex-exit-code` and the last
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

*The cwd was not stuck.* This was first written up here as a suspected CodeRex
defect, on the evidence that every Bash call began with `cd /tmp`. The stream
disproves it: the first `pwd`/`ls -la /app` runs in `/app`, and the `CurrentDir`
tool returns `/app`. `cd /tmp` was the model's own choice, and
`bash_tool.rs`'s child gets `ctx.cwd` on every call (`sandbox_shell.rs:66`) —
there is no persistent shell whose directory could drift. Recorded so the
suspicion is not raised twice.

Two CodeRex problems did fall out of this run: the single hard-coded WebSearch
backend and the missing closing event when a timeout ends a run. Both are now
fixed — see defects 5 and 7 in [CodeRex defects the benchmark
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
bit-rot failure and not a CodeRex result. It is listed as not measured rather
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
| `path-tracing` | ✅ resolved (4/4) | reconstructed the image at 0.996 similarity; recorded as a failure only because of a CodeRex shutdown defect, now fixed |
| `conda-env-conflict-resolution` | ❌ unresolved | 900 s timeout; `environment.yml` was fixed correctly, conda never finished |
| `pytorch-model-cli` | ⚠️ not measured | test phase starved by `apt-get` at 49 kB/s; pytest never printed a summary |
| `sanitize-git-repo.hard` | ⚠️ not measured | image build: `git clone dclm` died mid-transfer |

**`path-tracing` — a run that passed every test was recorded as a failure, and
the cause was CodeRex.** The four assertions all pass:

```
PASSED test_image_c_exists  test_image_compiles
PASSED test_runs_and_produces_output  test_image_similarity
```

The result frame says `subtype: success`, and the image it rendered scores
0.9959 against the target. Yet the trial is still `unknown_agent_error`,
because `coderex-exit-code` is `124`: the 900 s timer fired. The timeline
explains it — the `result` frame was written at 892.9 s, and the harness killed
the process at 900 s, seven seconds later.

Those seven seconds were not shutdown. With `CODEREX_LOG` on, the process
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
not measured rather than as a CodeRex failure.

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
- **The defect is CodeRex's.** The stream ends on a `user` (tool-result) frame
  and `coderex-exit-code` is 124 — exactly the shape recorded for
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
problem, not a CodeRex one.

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
about CodeRex.

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
artefact of the network, not a CodeRex result — the deliverable it grades was
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
finally ran. It still failed, for a reason that had nothing to do with CodeRex.
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
them is a CodeRex defect.

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

724 s of work, `is_error: true`, no kernel. Nothing in this trial is a CodeRex
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
and `/etc/profile.d/99-coderex-proxy.sh`. With
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

### Scoreboard so far

72 distinct tasks attempted (90 % of the 80 in the dataset), 41 resolved
(57 % of those attempted). `qemu-alpine-ssh`, `pytorch-model-cli`,
`run-pdp11-code`, `sanitize-git-repo.hard`, `play-zork`, `gpt2-codegolf`,
`hf-model-inference`, `download-youtube`, `extract-moves-from-video`,
`build-initramfs-qemu` and `get-bitcoin-nodes` count as attempted but not
resolved. Scored by distinct task, taking the best result where a task ran
more than once — several did, both to re-check a fix and because the same task
ran on DeepSeek and then on KIMI. The thirty that have never resolved:

`build-initramfs-qemu`, `build-linux-kernel-qemu`, `conda-env-conflict-resolution`,
`configure-git-webserver`, `cron-broken-network`, `count-dataset-tokens`,
`decommissioning-service-with-sensitive-data`, `download-youtube`,
`extract-moves-from-video`, `get-bitcoin-nodes`, `git-multibranch`,
`gpt2-codegolf`, `hf-model-inference`, `intrusion-detection`,
`jupyter-notebook-server`, `nginx-request-logging`, `path-tracing-reverse`,
`play-zork`, `polyglot-c-py`, `polyglot-rust-c`, `pytorch-model-cli`,
`qemu-alpine-ssh`, `raman-fitting`, `raman-fitting.easy`, `reshard-c4-data`,
`run-pdp11-code`, `sanitize-git-repo.hard`, `sanitize-git-repo`,
`tmux-advanced-workflow`, `vim-terminal-task`.

The eight never run:
`eval-mteb`, `eval-mteb.hard`, `fix-git`, `oom`, `qemu-startup`,
`security-vulhub-minio`, `solana-data`, `super-benchmark-upet`,
`train-fasttext`. Most need a dataset download, a multi-GB model, or a
prebuilt third-party image; `fix-git`'s fixture is gone from GitHub, and
`qemu-startup`'s image no longer builds (see the thirteenth batch).

| Failure class | Tasks |
|---|---|
| Dataset: instruction contradicts or omits what the test checks | `polyglot-c-py`, `nginx-request-logging`, `vim-terminal-task`, `decommissioning-service-with-sensitive-data`, `raman-fitting.easy`, `jupyter-notebook-server`, `intrusion-detection` (test greps the source for `logs/auth.log`; the script uses a variable and works), `build-initramfs-qemu` (expects `localhost login:`; the boot prompt is `initramfs login:` because the instruction never asks for a hostname), `reshard-c4-data` (test demands a flat `c4_reshard/` holding the original basenames after `revert.py` runs; the instruction only bounds files-per-folder, so a nested layout is legal and fails no stated rule) |
| Dataset: fixture is stale or no longer runs | `cron-broken-network` (example.com redesign), `fix-git` (repository deleted), `git-multibranch` (test assumes a repository state the instruction never specifies), `qemu-alpine-ssh` and `qemu-startup` (`debian:bullseye-slim` resolves `qemu 5.2+deb11u5`, deleted upstream — the image build 404s before the agent runs), `sanitize-git-repo.hard` (image build's `git clone` of `dclm` dies with `GnuTLS recv error`), `build-linux-kernel-qemu` (`tests/test_outputs.py` line 37-38 splits string concatenation so the `+` becomes unary plus and pytest cannot collect; once that parses, the image ships no `ramfs` though the instruction promises one, and the test's `gzip -9` then silently reuses the agent's archive — patched, see below) |
| Dataset: dependency unreachable | `incompatible-python-fasttext` and `...base_with_hint` (huggingface.co — patched, see below; **both pass once they build**), `reshard-c4-data` (image build downloads `allenai/c4` from huggingface.co — patched, see below), `count-dataset-tokens` (container cannot reach huggingface.co at all, though the host can), `raman-fitting` / `raman-fitting.easy` (pip slower than the 60 s Bash timeout, so numpy never installed), `pytorch-model-cli` (test phase `apt-get` at 49 kB/s), `build-linux-kernel-qemu` (apt from `ports.ubuntu.com` at 30-60 kB/s: the 3000 s budget went to installing the kernel build dependencies), `hf-model-inference` / `download-youtube` / `extract-moves-from-video` (need huggingface.co, youtube.com or GitHub releases; reachable now with `--task-proxy`, but the full budget went to fetching a multi-GB `torch` wheel, the video, or a `deno` toolchain) |
| Model behaviour | `configure-git-webserver` (key-only SSH, test wants a password), `sanitize-git-repo` (sanitized one file outside the allowed set), `tmux-advanced-workflow` (killed the harness's own tmux server), `path-tracing-reverse` (budget spent before writing the deliverable), `git-multibranch` (left the repository in a state its own test cannot pass), `polyglot-rust-c` (900 s on the technique, deliverable never written), `intrusion-detection` (also: no IP validation in the response script), `raman-fitting` (retried pip in the foreground ~150 s before backgrounding it), `conda-env-conflict-resolution` (correct yml, but spent the rest on a conda solve/download that never finished), `play-zork` (spent the full budget exploring the game and never wrote the deliverable), `gpt2-codegolf` (84 edits to one C file, landed at 5030 bytes against a 5000-byte limit when the budget ran out), `run-pdp11-code` (identified the PDP-11 `a.out` and built `apout` correctly, but its emulated output is off by 96 and it never wrote `/app/out.txt`), `download-youtube` (right video from an archive.org mirror, last 10 s misaligned by ~4 frames: SSIM 0.948 against a 0.95 threshold), `extract-moves-from-video` (spent the budget building a `deno` transcription toolchain and never wrote `solution.txt`), `hf-model-inference` (batch 15 saved the model without `tokenizer.json`), `get-bitcoin-nodes` (the genesis coinbase transaction 404s while a recent one returns 200) |
| Provider | `incompatible-python-fasttext` in `batch7b` (401, key invalid — the run itself passed) |
| Harness/plumbing | `tmux-advanced-workflow` (grading script starved on `apt-get update`) |
| CodeRex defect | none outstanding — the eight found are fixed, two more are known and unfixed (see [Known, unfixed](#known-unfixed)) |

## CodeRex defects the benchmark exposed

All seven were real, and all seven are fixed on `main`:

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

   The fix makes the backend list configurable and ordered. `CODEREX_SEARCH_BACKENDS`
   is a comma-separated chain (`CODEREX_SEARCH_BACKEND` for a single entry),
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
   900 s, and `coderex-exit-code` is `124` — the timer, not the tests.

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
   result, `coderex-exit-code: 124`, no `result` line. The harness therefore
   reports `unknown_agent_error`, which is what a crash looks like — a reader
   cannot tell "ran out of time" from "broke".

   Three things were wrong, and all three are fixed:

   1. **The exit code contradicted the log.** The handler printed "shutting
      down gracefully" and exited 0; the harness's `timeout` wrapper turned
      that into 124. Anything reading the exit code saw a timeout, anything
      reading the log line saw a clean shutdown.
   2. **The reason was only logged, never emitted.** `tracing` defaults to
      `CODEREX_LOG=off` and writes to stderr, so even that one line was
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

### Known, unfixed

- **`UNKNOWN_CONTEXT_WINDOW` is still 32 000.** For a model CodeRex does not
  recognise, it compacts against a 32 K guess. The benchmark runs worked around
  this with `CODEREX_CONTEXT_WINDOW=131072`; the guess itself is unchanged, so
  an unknown model with a larger window still compacts far too early.
- **Compaction can burn its retry budget on a no-op.** Observed at the 32 K
  guess: a pass reported success while removing 35 tokens and *growing* the
  message list (39 → 44 → 47) before the turn hard-stopped with
  "context window is full".

## Local fixture patches

The cached dataset at `~/.cache/terminal-bench/terminal-bench-core/0.1.1` is a
copy, not the upstream source, and six of its tasks have been edited so they
can run at all on this machine. Every edit is recorded here so a result can be
traced back to the fixture it was measured against, and each edited file has a
`.orig` sibling holding the pristine version.

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
