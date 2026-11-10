# Benchmarks: what Codape can be measured against

Codape is a terminal coding agent, so the benchmarks worth its time are the ones
that hand it a machine and grade the state of that machine afterwards. This file
records which of those exist, which ones this repository can actually run today,
and which ones would need a new runner before a score means anything.

The short version: **Terminal-Bench is the only benchmark with a full recorded
score** (72 of 75 attempted tasks resolved — see
[terminal-bench.md](terminal-bench.md)). SWE-bench's runner now runs end-to-end
and has two small recorded subsets (see
[SWE-bench run history](#swe-bench-run-history)), but no Verified score yet.
Everything else below needs work first.

## What runs today

| Benchmark | Size | Runner in this repository | Recorded score |
|---|---|---|---|
| [Terminal-Bench](https://www.tbench.ai/) | 80 tasks in `terminal-bench-core==0.1.1` | `scripts/external-benchmarks/terminal-bench/` | Yes — [terminal-bench.md](terminal-bench.md) |
| [SWE-bench Verified](https://www.swebench.com/verified.html) | 500 | `scripts/external-benchmarks/swebench/` | Subset only — 3/3 (`verified-3repo-20261110-112457`); see [run history](#swe-bench-run-history) |
| [SWE-bench Lite](https://www.swebench.com/lite.html) | 300 | same runner (`--dataset lite`) | No |
| [SWE-bench Multilingual](https://www.swebench.com/multilingual.html) | 300 | same runner (`--dataset multilingual`) | No — alias and validation ready; a one-instance smoke run is in progress |
| Cassette benchmark | 17 mock cases (3 more live) | `scripts/test-code-agent-benchmark.sh` | In CI, every push |

The first four are research instruments: Docker, network, paid model tokens,
hours of wall clock. They are manual tools for release notes, deliberately kept
out of required checks — the reasoning is in
[`scripts/external-benchmarks/README.md`](../scripts/external-benchmarks/README.md#why-this-is-not-in-ci).

The fourth is different in kind. `scripts/test-code-agent-benchmark.sh` replays
recorded LLM cassettes against fixture projects and asserts on the resulting
file changes, so it is deterministic, key-free, and runs in CI. It measures the
harness (does a turn edit the right file, does review-only leave sources alone,
does a large-repo case finish) — not model capability. Use it to catch the
regressions; use the external benchmarks to measure the agent.

```bash
scripts/external-benchmarks/terminal-bench/run.sh --dry-run --limit 1
scripts/external-benchmarks/swebench/run.sh --dry-run --limit 1
```

`--dry-run` needs no Docker, no key, and no downloads. Start there.

## Pins

A score means nothing without the version it was produced at. These are the pins
the runners read; they live in the two runners and are listed with their file
locations in the external README.

| Pin | Value |
|---|---|
| SWE-bench harness | `swebench==5.0.2` |
| SWE-bench Verified revision | `78f471bf655a3137b2e8a75af1501690ec009ec3` |
| SWE-bench Lite revision | `b0dde1093fe417d83b7184254edf8199c1f0dff5` |
| SWE-bench Multilingual revision | `846e647b9f33c0b51b739d005d13d85493c9af09` |
| Terminal-Bench harness | `terminal-bench==0.2.18` |
| Terminal-Bench dataset | `terminal-bench-core==0.1.1` |
| Codape in containers | `v0.2.0` (`x86_64-unknown-linux-musl` for SWE-bench) |

Do not compare runs that differ in model ID, harness version, or dataset
revision. Re-running the same named subset is the only variance measurement
worth quoting.

## SWE-bench run history

The runner is exercised end-to-end, but only on named subsets. Neither entry
below is a SWE-bench Verified score: both were chosen by hand from the
12-repository list, not sampled at random, and the issues are public.

### 2026-11-10 — pipeline check, 1 instance (1/1 resolved)

- run-id: `verified-requests-1142-20261110-111435`
- instance: `psf__requests-1142`
- model: `deepseek-v4.1-flash` through an Anthropic-compatible gateway
- result: **1/1 resolved** by the official `swebench==5.0.2` harness;
  `error_instances: 0`, `empty_patches: 0`
- timings: generation 305 s (including the image pull), evaluation 19 s,
  total 324 s
- patch: 868 548 bytes, of which the real fix is
  `requests/models.py:prepare_content_length`; the rest is `build/lib/` output
  from the agent running `setup.py build`

This run only establishes that generation, patch extraction, and official
grading work on Linux against the pinned image.

### 2026-11-10 — 3-repository subset (3/3 resolved)

- run-id: `verified-3repo-20261110-112457`
- instances: `pallets__flask-5014`, `pytest-dev__pytest-6202`,
  `pylint-dev__pylint-6528`
- model: `deepseek-v4.1-flash` through the same gateway
- result: **3/3 resolved**; `unresolved_ids: []`, `error_instances: 0`,
  `empty_patches: 0`
- timings: generation 2 508 s, evaluation 70 s, total 2 578 s (43 minutes)

| Instance | Agent time | Patch bytes | Agent outcome |
|---|---|---|---|
| `pallets__flask-5014` | 123.7 s | 1 338 | `success` |
| `pytest-dev__pytest-6202` | 1 369.9 s | 621 503 | `success` |
| `pylint-dev__pylint-6528` | 813.5 s | 2 969 | `success` |

The pytest patch also modified `testing/test_collection.py` and downloaded four
`.whl` files into `/testbed`; both were captured by `git add -A`. The official
test patch still applied and the instance resolved, but patch hygiene is a
failure mode to watch when the subset gets larger. The flask and pylint patches
contain only source changes.

What these runs do **not** establish:

- A Verified pass@1. Three hand-picked instances are a smoke test; the 95 %
  Wilson interval on 3/3 starts around 0.44.
- Contamination resistance. All three issues are public, so their fixes may be
  in the model's training data.
- Scale. The 12-repository subset needs an estimated 40–60 GB of image storage
  and was not attempted here; the full 500-instance set was not attempted
  either. The runner generates instances serially, so a full set is a
  multi-day job on one machine.

## SWE-bench variants: an alias, not a runner

The SWE-bench family shares one dataset shape (`instance_id`, `problem_statement`,
`patch`, `test_patch`, `FAIL_TO_PASS`, `PASS_TO_PASS`) and one official grading
harness. Adding a variant is therefore mostly a matter of pinning its dataset
revision in `DATASETS` in
[`run_swebench.py`](../scripts/external-benchmarks/swebench/run_swebench.py) —
`verified`, `lite`, and `multilingual` are there. What that does **not** solve
is the images: each instance still pulls its own container, and Multimodal
additionally needs browser plumbing.

| Variant | Size | Notes | Work needed |
|---|---|---|---|
| Full | 2 294 | The original test split, 12 Python repositories. Superseded by Verified for headline numbers. | Alias only |
| [Multilingual](https://www.swebench.com/multilingual.html) | 300 tasks, 41 repositories in the pinned revision, 9 languages | C, C++, Go, Java, JS/TS, PHP, Ruby, Rust, Python. Tests Codape's shell/tooling in build systems it was not written against — a genuinely different measurement from the Python-only sets. | Alias added; no runner-side non-Python spec table is needed (see below). End-to-end run pending |
| [Multimodal v2](https://www.swebench.com/multimodal.html) | 480 (v2 removed flaky/ungradeable tests from the original 517) | Issues contain screenshots, mockups, and diagrams; graded in a browser-driven environment. | Alias plus image/browser setup; Codape reads images, but the container plumbing is new work |

Multilingual is the variant with the most information per unit of effort for
this project: it is the only SWE-bench shape that exercises a Rust or Go
toolchain inside the graded container.

### The non-Python spec table lives in the dataset

Older `swebench` releases shipped `MAP_REPO_VERSION_TO_SPECS`, a harness table
of install/test commands and log parsers keyed by repository and version. That
is what the earlier version of this file meant by "harness support for the
non-Python spec table". `swebench==5.0.2` no longer has it: each parquet row
carries `eval_script`, `log_parser`, `eval_type`, and `image`, and the official
`make_test_spec` reads those fields. The pinned Multilingual revision builds
all 300 specs this way; the 19 parsers it names are all present in the
harness's `PARSER_REGISTRY`.

The runner therefore adds an alias and a small language table, not a second set
of build recipes: `MULTILINGUAL_LANGUAGE_BY_PARSER` labels each row for the
summary and rejects an unknown parser before any model call. It also validates
the selected rows' required columns. A full Multilingual score still requires
the images (one per instance) and paid model calls, so the alias being present
is not the same as a recorded result.

Composition of the pinned revision, by the language the runner records:

| Language | Instances |
|---|---|
| C | 25 |
| C++ | 12 |
| Go | 42 |
| Java | 43 |
| JavaScript/TypeScript | 43 |
| PHP | 43 |
| Python | 5 |
| Ruby | 44 |
| Rust | 43 |

## Benchmarks that need a new runner

All of these are Docker-based and all of them grade by running the project's own
tests, so the extraction step in the existing SWE-bench runner (stage 5:
`git add -A` plus `git diff --cached --binary <base>`) is reusable. What differs
is how an instance is built, started, and scored.

| Benchmark | What it measures | Size | Why it needs new work |
|---|---|---|---|
| [Multi-SWE-bench](https://github.com/multi-swe-bench/multi-swe-bench) | Issue fixing in Java, TypeScript, JavaScript, Go, Rust, C, C++ — 2 456 candidates curated down to 1 632 instances by 68 annotators. NeurIPS 2025 D&B. Also ships `mini` (400, 8 languages) and `flash` (300) subsets. | 1 632 | Own harness (`make install`, `multi-swe-bench eval`) and its own image set; the official `swebench` harness does not grade it |
| [SWE-rebench v2](https://swe-rebench.com/) | The same shape, but instances are selected by **time window** rather than fixed at publication — the current window (15/05/2026 – 01/07/2026) holds 111 problems from 65 repositories. Trades comparability for contamination resistance: a model cannot have memorized a bug filed after its training cut-off. | Rolling | Own data pipeline and harness; the useful number is `Resolved Rate` at a stated window, so the window must be recorded with the score |
| [Commit0](https://github.com/commit-0/commit0) | Building a library **from scratch**: 57 Python libraries, given the spec and docs but not the implementation, rebuilt and graded by the project's own unit tests, lint, and type checks. | 57 libraries | Own environment (`pip install commit0`, `commit0 build/test/eval`); there is no starting repository, so the patch-extraction model does not apply |
| [ProgramBench](https://programbench.com/) | Recovering a program from a **compiled binary plus its documentation** — 200 tasks, hidden behavioral tests, from Meta Superintelligence Labs, Stanford, and Harvard. Leaderboard reports both fully resolved and "almost" (≥ 95 % of behavioral tests). | 200 | Own harness, own DockerHub images, behavioral rather than unit-test grading |
| [CodeClash](https://codeclash.ai/) | Goal-oriented software engineering rather than tasks: agents start from a high-level objective and evolve a codebase over rounds, scored by relative competition in arenas (Halite, Poker, CoreWar, RobotRumble, Robocode, BattleSnake). Ranked by Elo. | 6 arenas | Own arenas and multi-round protocol; an agent is run repeatedly as a competitor, not once per instance |

Two of these are worth more than the others to this project:

- **ProgramBench** tests the thing Codape is built for (explore a filesystem,
  run binaries, read docs) with no repository to lean on.
- **SWE-rebench v2** is the only benchmark here that makes a contamination story
  plausible, and its result is a percentage at a *dated window*, which is a
  stronger claim than a fixed-split number.

## Not a fit

| Benchmark | Why not |
|---|---|
| [OSWorld](https://os-world.github.io/) | GUI desktop agent — mouse, screen, applications. A terminal coding agent cannot be placed on it without a different tool surface, so a score would measure a wrapper |
| GAIA | General-assistant tasks (web research, file wrangling), not software engineering; the failure modes it exposes are not the ones Codape can fix |
| [SWE-smith](https://swesmith.com/) | A toolkit for **generating** synthetic broken repositories for training, not a scored benchmark. Useful if the cassettes are ever regenerated at scale; not a number to publish |

## Reading a score

Three things to keep attached to any number reported from these benchmarks:

- **Subset size.** One instance is 0 % or 100 %. A 20-instance SWE-bench run
  validates the pipeline, not the agent. Record the instance IDs and run ID
  alongside the percentage.
- **Contamination.** Every benchmark here is public; SWE-bench Verified is
  human-validated and Terminal-Bench tasks are recent, but neither proves the
  model has not seen the task. The runners tell the agent not to look issues up
  online; a memorized fix is still reproducible. Treat scores as upper bounds.
- **Whose failure it was.** A benchmark score of 0 can mean the agent harness
  broke, or the model declined the task. [terminal-bench.md](terminal-bench.md)
  exists mostly to keep the frame subtype and the test's own verdict apart —
  the same discipline applies to any runner added here.

Cost is unbounded by default. Bound it with `--max-turns` and `--timeout`, use a
cheap model for plumbing checks, and estimate spend from the provider's billing
dashboard: Codape 0.2.0 emits a `result` frame but does not populate `usage`, so
both runners print `tokens/cost: unavailable` rather than inventing numbers.
