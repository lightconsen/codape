# Benchmarks: what Codape can be measured against

Codape is a terminal coding agent, so the benchmarks worth its time are the ones
that hand it a machine and grade the state of that machine afterwards. This file
records which of those exist, which ones this repository can actually run today,
and which ones would need a new runner before a score means anything.

The short version: **Terminal-Bench is the only benchmark with a full recorded
score** (73 of 75 attempted tasks resolved — see
[terminal-bench.md](terminal-bench.md)), and an eight-task subset was run three
times on 2026-11-10 to measure repeatability: 6/8 in each repeat, 18/24 (75 %)
overall. SWE-bench's runner now runs end-to-end
on Verified, Lite, and Multilingual, with small hand-picked subsets recorded for
each, a 24-instance repository-stratified Verified sample on 2026-11-10
(**21/24**, one instance blocked by the evaluation container's TLS setup), and a
12-instance, nine-language Multilingual sample on 2026-11-11 (**9/12**) — see
[SWE-bench run history](#swe-bench-run-history). ProgramBench has a runner and
one solved instance, but a single instance is a pipeline check, not a benchmark
score.
Everything else below needs work first.

## What runs today

| Benchmark | Size | Runner in this repository | Recorded score |
|---|---|---|---|
| [Terminal-Bench](https://www.tbench.ai/) | 80 tasks in `terminal-bench-core==0.1.1` | `scripts/external-benchmarks/terminal-bench/` | Yes — 73/75 best-of-runs; 2026-11-10 revalidation 5/10; eight-task reliability, three repeats: **6/8, 6/8, 6/8** — [terminal-bench.md](terminal-bench.md) |
| [SWE-bench Verified](https://www.swebench.com/verified.html) | 500 | `scripts/external-benchmarks/swebench/` | Sample — **21/24** on a 24-instance, 12-repository stratified subset (`swe-verified-24-20261110-b1..b6`); earlier hand-picked subsets 3/3, 2/3; see [run history](#swe-bench-run-history) |
| [SWE-bench Lite](https://www.swebench.com/lite.html) | 300 | same runner (`--dataset lite`) | Subset only — 2/3 (`lite-batch1-20261110-122725`); see [run history](#swe-bench-run-history) |
| [SWE-bench Multilingual](https://www.swebench.com/multilingual.html) | 300 | same runner (`--dataset multilingual`) | Sample — **9/12** on a 12-instance, 9-language stratified subset (`swe-multilingual-12-20261110-b1..b3`); earlier pipeline check 1/1; see [run history](#swe-bench-run-history) |
| [ProgramBench](https://programbench.com/) | 201 instances in `programbench==1.2.5` | `scripts/external-benchmarks/programbench/` | Sample — 0/3 solved on `figlet` (0/1044, compile_failed), `pixterm` (60/430), `zip-password-finder` (92/680); `caps-log` improved from 0 (`compile_failed`, compaction loop) to 71 once the served window was passed in; earlier `cmatrix` pipeline check 1/1; see [run history](#programbench-run-history) |
| Cassette benchmark | 17 mock cases (3 more live) | `scripts/test-code-agent-benchmark.sh` | In CI, every push |

The first five are research instruments: Docker, network, paid model tokens,
hours of wall clock. They are manual tools for release notes, deliberately kept
out of required checks — the reasoning is in
[`scripts/external-benchmarks/README.md`](../scripts/external-benchmarks/README.md#why-this-is-not-in-ci).

The sixth is different in kind. `scripts/test-code-agent-benchmark.sh` replays
recorded LLM cassettes against fixture projects and asserts on the resulting
file changes, so it is deterministic, key-free, and runs in CI. It measures the
harness (does a turn edit the right file, does review-only leave sources alone,
does a large-repo case finish) — not model capability. Use it to catch the
regressions; use the external benchmarks to measure the agent.

```bash
scripts/external-benchmarks/terminal-bench/run.sh --dry-run --limit 1
scripts/external-benchmarks/swebench/run.sh --dry-run --limit 1
scripts/external-benchmarks/programbench/run.sh --dry-run
```

`--dry-run` needs no Docker, no key, and no downloads. Start there.

## Pins

A score means nothing without the version it was produced at. These are the pins
the runners read; they live in the three runners and are listed with their file
locations in the external README.

| Pin | Value |
|---|---|
| SWE-bench harness | `swebench==5.0.2` |
| SWE-bench Verified revision | `78f471bf655a3137b2e8a75af1501690ec009ec3` |
| SWE-bench Lite revision | `b0dde1093fe417d83b7184254edf8199c1f0dff5` |
| SWE-bench Multilingual revision | `846e647b9f33c0b51b739d005d13d85493c9af09` |
| Terminal-Bench harness | `terminal-bench==0.2.18` |
| Terminal-Bench dataset | `terminal-bench-core==0.1.1` |
| ProgramBench harness | `programbench==1.2.5` |
| ProgramBench cleanroom image | `task_cleanroom_v6` |
| Codape in containers | `v0.2.0` (`x86_64-unknown-linux-musl`) |

Do not compare runs that differ in model ID, harness version, or dataset
revision. Re-running the same named subset is the only variance measurement
worth quoting.

## SWE-bench run history

The runner is exercised end-to-end, but only on named subsets. None of the
entries below is a SWE-bench Verified or Lite score: each instance was chosen by
hand rather than sampled, and the issues are public.

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

### 2026-11-10 — Verified batch 1 (2/3 resolved)

- run-id: `verified-batch1-20261110-122725`
- instances: `django__django-14007`, `astropy__astropy-14369`,
  `mwaskom__seaborn-3187`
- model: `deepseek-v4.1-flash` through the same gateway
- result: **2/3 resolved**; `resolved_ids` are django-14007 and seaborn-3187;
  `error_instances: 0`, `empty_patches: 0`
- timings: generation 1 599.8 s, evaluation 67.2 s, total 1 667.0 s

| Instance | Agent time | Patch bytes | Agent outcome |
|---|---|---|---|
| `django__django-14007` | 230.2 s | 2 008 | `success` |
| `astropy__astropy-14369` | 271.8 s | 13 727 | `success` |
| `mwaskom__seaborn-3187` | 860.5 s | 1 668 | `success` |

Astropy is the unresolved instance: the agent's patch made two new
`test_cds_grammar` cases pass, but `test_cds_grammar_fail[km/s.Mpc-1]` still
reports `DID NOT RAISE <class 'ValueError'>`, and PASS_TO_PASS
`test_cds_grammar_fail[/s.Mpc]` fails the same way. The agent widened the
grammar to accept products after `/` and a leading `/`, while the test requires
the opposite: slash chains are valid (`km/s/Mpc` means `km / (s Mpc)`) but an
unparenthesized product in the denominator must be rejected.

The miss is implementable, not an impossible task. A corrected grammar patch —
one extra nonterminal that allows chained divisions in the denominator but not
products, plus the regenerated `cds_parsetab.py` — was replayed without model
calls and resolved 1/1 in the official harness
(`verified-astropy14369-grammarfix-20261110-140245`): 3/3 FAIL_TO_PASS and
732/732 PASS_TO_PASS. It is a failure of reading the negative test contract and
verifying both directions, not of coding ability.

A best-practice variant models the denominator as a left-recursive list of
units rather than a right-recursive product, documents the parenthesization
rule in the grammar, and ships the regenerated parser table
(`verified-astropy14369-grammarfix-best-20261110-141456`, 13 121 bytes). It
also resolves 1/1, passes all 738 tests in `test_format.py`, and leaves the
wider units suite with the same 17 pre-existing environment failures as the
base: 2 974 passed on the base versus 2 980 with the upstream test patch. Both
fixes are therefore within the agent's coding ability; the gap is the
discipline to derive the change from both the positive and the negative tests.

### 2026-11-10 — Lite batch 1 (2/3 resolved)

- run-id: `lite-batch1-20261110-122725`
- instances: `psf__requests-1963`, `pylint-dev__pylint-5859`,
  `pytest-dev__pytest-11143`
- model: `deepseek-v4.1-flash` through the same gateway
- result: **2/3 resolved**; `resolved_ids` are pylint-5859 and pytest-11143;
  `error_instances: 0`, `empty_patches: 0`
- timings: generation 567.3 s, evaluation 65.1 s, total 632.4 s

| Instance | Agent time | Patch bytes | Agent outcome |
|---|---|---|---|
| `psf__requests-1963` | 270.1 s | 13 544 | `success` |
| `pylint-dev__pylint-5859` | 95.9 s | 298 944 | `success` |
| `pytest-dev__pytest-11143` | 37.2 s | 69 266 | `success` |

Requests is the unresolved instance: all seven FAIL_TO_PASS tests passed, but
two PASS_TO_PASS tests failed with
`ssl.SSLCertVerificationError: CERTIFICATE_VERIFY_FAILED: unable to get local
issuer certificate` against live external HTTPS endpoints. That is
environmental, not an agent regression: a comment-only baseline patch
(`lite-requests1963-comment-baseline-20261110-131751`, 308 bytes, no behavior
change) reproduced exactly the same two PASS_TO_PASS failures, while failing
only `test_requests_are_updated_each_time` among the FAIL_TO_PASS cases — the
one the agent's patch fixed. The official harness still counts the instance as
unresolved; grading it would require fixing the pinned image's trust store or
the test's network path.

The pylint and pytest patch sizes are mostly noise: those images check out
files as executable, and the runner's `git add -A` staged thousands of
`old mode 100644 / new mode 100755` entries around a single content hunk (2 064
entries for pylint, 577 for pytest). The content hunk is what the harness
graded, and the test patch still applied. The fix is in the runner but postdates
these patches: reset the index to the base commit (mixed reset, worktree
untouched) and run both `git add` and `git diff` with `core.fileMode=false`.
The reset is required because the image's index already records mode 755 — the
requests image, for example, has 118 such entries before the agent runs. After
the fix, a 308-byte comment-only change to that image extracted as a 308-byte
patch with zero mode entries, so new runs should produce small patches.

### 2026-11-10 — Multilingual pipeline check, 1 instance (1/1 resolved)

- run-id: `multilingual-jq2235-20261110-123314`
- instance: `jqlang__jq-2235` (C; `parse_log_jq`)
- model: `deepseek-v4.1-flash` through the same gateway
- result: **1/1 resolved**; `error_instances: 0`, `empty_patches: 0`
- timings: generation 740.6 s, evaluation 41.4 s, total 781.9 s
- patch: 1 679 bytes in `src/main.c`, adding `--raw-output0` option handling
  and rejecting NUL bytes in raw output

This is the first end-to-end non-Python run. It exercises the data-driven path
— the row's `eval_script` and `log_parser`, the official `make_test_spec`, and
the container image — on a C repository. It is not a Multilingual score: one
instance is 0 or 100 %, and the issue is public.

### 2026-11-10/11 — Verified sample, 24 instances across 12 repositories (**21/24 resolved**)

- run-ids: `swe-verified-24-20261110-b1` … `-b6` (six batches of four)
- sample: 24 instances drawn with seed `20261110`, stratified by repository —
  two instances from each of ten repositories, three from `django`, one from
  `flask`; the exact IDs are the keys of each batch's `predictions.jsonl`
  under `target/external-benchmarks/swebench/`
- dataset: `SWE-bench/SWE-bench_Verified` @ `78f471bf655a3137b2e8a75af1501690ec009ec3`
- model: `deepseek-v4.1-flash` through the same gateway
- protocol: 1 800 s agent timeout, 300 turns, two official-harness workers; each
  batch's images deleted after grading to stay inside the host's 50 GB disk
- result: **21/24 resolved (87.5 %)**; `error_instances: 0`,
  `empty_patches: 0`, `infra_failure_instances: 0`; 95 % Wilson interval
  [0.69, 0.96]
- timings: generation 9 367 s (2.6 h), evaluation 475 s; per batch 21.6, 52.4,
  17.8, 26.5, 18.4 and 27.4 minutes

| Repository | Resolved | Instances |
|---|---|---|
| astropy | 2/2 | 14309, 8707 |
| django | 3/3 | 11951, 14752, 15569 |
| matplotlib | 2/2 | 25287, 26208 |
| mwaskom (seaborn) | 1/2 | 3069 ✅, 3187 ❌ |
| pallets (flask) | 1/1 | 5014 |
| psf (requests) | 1/2 | 5414 ✅, 1921 ❌ |
| pydata (xarray) | 1/2 | 4687 ✅, 6938 ❌ |
| pylint-dev | 2/2 | 6903, 7277 |
| pytest-dev | 2/2 | 5631, 7982 |
| scikit-learn | 2/2 | 14983, 25973 |
| sphinx-doc | 2/2 | 7889, 8551 |
| sympy | 2/2 | 15017, 24539 |

Every patch was a non-empty source diff; the shortest was 475 bytes
(`pytest-7982`) and the longest 3 130 bytes (`seaborn-3069`). The three
unresolved split into two genuine model misses and one environment defect.

**`xarray__xarray-6938` — fixed at the wrong layer.** The model changed
`Dataset.swap_dims()` to copy an `IndexVariable` before assigning `dims`. The
graded test,
`xarray/tests/test_variable.py::TestIndexVariable::test_to_index_variable_copy`,
calls `IndexVariable.to_index_variable()` directly and asserts the result is a
different object; the intended change is in `xarray/core/variable.py`. The
symptom the issue described disappears through `swap_dims`, but the API
contract the test checks does not. 1 of 431 tests failed; everything else
passed.

**`mwaskom__seaborn-3187` — a partial fix.** The model suppressed
matplotlib's offset and scientific notation in
`seaborn/_core/scales.py`, gated on `isinstance(formatter, ScalarFormatter)`.
That covers the `Plot` path — `TestLegend::test_legend_has_no_offset` passed —
but `relplot()` builds its legend through `seaborn/utils.py`, which the model
did not touch, so
`TestRelationalPlotter::test_legend_has_no_offset` still reads `2.5` where it
expects a value above `1e7`. The upstream fix changes both files.

**`psf__requests-1921` — the model's fix works; the container cannot grade
it.** The model removed `None`-valued keys from the merged settings, and all
six `FAIL_TO_PASS` tests passed, including the new
`test_headers_on_session_with_None_are_not_sent`. The instance is still marked
unresolved because two unrelated `PASS_TO_PASS` tests fail with
`SSLError: CERTIFICATE_VERIFY_FAILED` against `httpbin.org`
(`test_mixed_case_scheme_acceptable`, `test_pyopenssl_redirect`). To separate
the patch from the environment, the dataset's own gold patch was re-graded
through the official harness (`--eval-only`,
`gold-requests-1921-20261111`): it also scores **0/1**, with the same three
failures. The instance is therefore not scoreable in this container, and the
agent-side result is 22 of the 23 scoreable instances (95.7 %, Wilson
[0.79, 0.99]).

The gold-patch re-grade is worth repeating whenever a miss looks suspicious:
it costs one image pull and one evaluation, and it distinguishes a model error
from a broken evaluation environment without spending model tokens.

### 2026-11-11 — Multilingual sample, 12 instances across 9 languages (**9/12 resolved**)

- run-ids: `swe-multilingual-12-20261110-b1` … `-b3` (three batches of four)
- sample: 12 instances, at least one per language and four Ruby instances
  (`faker` ×2, `fastlane` ×2); the exact IDs are in each batch's
  `predictions.jsonl` under `target/external-benchmarks/swebench/`
- dataset: `SWE-bench/SWE-bench_Multilingual` @ `846e647b9f33c0b51b739d005d13d85493c9af09`
- model and protocol: identical to the Verified sample — 1 800 s agent timeout,
  300 turns, two official-harness workers, each batch's images deleted after
  grading
- result: **9/12 resolved (75 %)**; `error_instances: 0`,
  `empty_patches: 0`, `infra_failure_instances: 0`; 95 % Wilson interval
  [0.47, 0.91]
- timings: generation 5 175 s (1.4 h), evaluation 315 s; per batch 42.2, 21.6
  and 27.7 minutes

| Language | Resolved | Instances |
|---|---|---|
| C | 1/1 | `redis__redis-11734` |
| C++ | 0/1 | `fmtlib__fmt-3750` ❌ |
| Go | 0/1 | `caddyserver__caddy-6411` ❌ |
| Java | 1/1 | `apache__lucene-12626` |
| JavaScript/TypeScript | 1/1 | `babel__babel-16130` |
| PHP | 1/1 | `laravel__framework-51195` |
| Python | 1/1 | `micropython__micropython-13039` |
| Ruby | 3/4 | `faker-ruby__faker-2705`, `faker-ruby__faker-2970`, `fastlane__fastlane-19765` ✅; `fastlane__fastlane-19207` ❌ |
| Rust | 1/1 | `astral-sh__ruff-15443` |

Except for micropython, every patch was a small source diff between 526 bytes
(`faker-2970`) and 4 320 bytes (`babel-16130`). The three misses are all
interpretation errors on top of a plausible-looking change:

**`caddyserver__caddy-6411` (Go) — trimmed every trailing newline instead of
one.** The model replaced `return string(body), true` in `fileReplacementProvider`
with `strings.TrimRight(string(body), "\r\n")`, so a file ending in `foo\n\n`
becomes `foo`. The upstream change removes exactly one `\n` and then at most one
`\r` (`bytes.TrimSuffix` twice). The FAIL_TO_PASS test reads a fixture named
`foo_with_multiple_trailing_newlines.txt`, expects `"foo\n"`, and got `"foo"`.
All PASS_TO_PASS tests still passed; the change was simply too aggressive.

**`fmtlib__fmt-3750` (C++) — reversed the issue's direction.** The model gated
`loc_writer`'s integer path on `specs.type == none || dec`, i.e. it restricted
localization to decimal. The issue asks for the opposite: grouping in hex,
binary and octal under the `L` specifier. `format_test.format_locale` expects
`"7,5bc,d15"` for `{:Lx}`, a grouped `{:#Lb}`, and `"    30,071"` for
`{:10Lo}`; the patch produces the ungrouped `"75bcd15"`, `"-0b111010..."`
and `"     30071"`. The tests that already passed — `format_int_locale`,
`format_facet_grouping` — kept passing, which is probably why the agent
concluded it was done.

**`fastlane__fastlane-19207` (Ruby) — changed the calling convention instead of
escaping the argument.** `zip_command` returns an array of arguments. The
upstream fix shellescapes `output_path` and `basename` inside that array and
leaves `Actions.sh(zip_command)` as it was; the model instead splatted the array
into `Actions.sh(*zip_command)`. Every spec asserts
`expect(Fastlane::Actions).to receive(:sh).with([...])` — a single array
argument — so the new shell-escape FAIL_TO_PASS test still fails (nothing was
escaped) and six PASS_TO_PASS tests regress on the arity change alone. The patch
also adds a stray `zip-action-fix.patch` copy of itself to the repository.

**`micropython__micropython-13039` is a patch-hygiene warning, not a code
result.** It resolved, but with a 9.4 MB, 688-file patch: 686 paths under
`.venv/` and a built binary `tests/unix/ffi_lib.so`. The actual fix is one line
in `py/objslice.c` (`mp_obj_int_get_checked` → `mp_obj_get_int`). The harness
applied and graded it correctly, but a patch that sweeps a virtualenv into the
diff would be rejected by most review processes; the runner should probably
filter ignored paths before writing `predictions.jsonl`.

What these runs do **not** establish:

- A leaderboard-comparable SWE-bench pass@1. Both samples are random but
  small — 21/24, Wilson [0.69, 0.96] on Verified; 9/12, Wilson [0.47, 0.91] on
  Multilingual — and each is one run of one model. Lite still has only a
  three-instance hand-picked subset.
- Per-language rates. The Multilingual sample has one instance for each
  language except Ruby, so a row like "Go 0/1" records what happened once, not
  how the agent does on Go.
- Contamination resistance. All selected issues are public, so their fixes may
  be in the model's training data.
- Scale. The full 500-instance Verified set and the 300-instance Lite and
  Multilingual sets were not attempted. The runner generates instances
  serially, images are 1–5 GB each, and this 50 GB host had to delete every
  batch's images before pulling the next batch's.

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
| [Multilingual](https://www.swebench.com/multilingual.html) | 300 tasks, 41 repositories in the pinned revision, 9 languages | C, C++, Go, Java, JS/TS, PHP, Ruby, Rust, Python. Tests Codape's shell/tooling in build systems it was not written against — a genuinely different measurement from the Python-only sets. | Alias added; no runner-side non-Python spec table is needed (see below). 12-instance, 9-language sample **9/12** (`swe-multilingual-12-20261110-b1..b3`); earlier pipeline check 1/1 |
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

## ProgramBench run history

ProgramBench is the closest of these benchmarks to Codape's own surface: there
is no repository and no issue text, only a reference executable and the
documentation that ships with it. The official package supplies the cleanroom
image, the hidden behavioural test branches, and the scorer; the runner adds
the pinned Codape install, the network policy, and submission extraction.

One instance was selected by hand, `abishekvashok__cmatrix.5c082c6` (`cmatrix`,
C, easy), because it builds with the cleanroom's stock gcc and does not need a
downloaded model or dataset. It is a pipeline check, not a ProgramBench score.

### 2026-11-10 — `cmatrix` pipeline check, **1/1 solved (official ✅)**

- run-id: `pb-cmatrix-20261110-r2`
- instance: `abishekvashok__cmatrix.5c082c6` (C, easy)
- model: `deepseek-v4.1-flash` through an Anthropic-compatible gateway
- image: `programbench/abishekvashok_1776_cmatrix.5c082c6:task_cleanroom_v6`
- agent budget: 7 200 s, 500 turns, 4 CPUs, 8 GB
- network: Docker internal network; only `opencode.ai:443` is reachable through
  the runner's CONNECT proxy
- agent: 175 assistant turns, 160 `Bash` calls, about 69 minutes
- submission: `src/cmatrix.c` (16 KB), `compile.sh`, probe scripts; built with
  the cleanroom's gcc and ncurses
- raw tests: 768/769
- official: 506/506 scored tests, score 100, ✅ solved

The raw count includes 263 tests the task marks ignored. The only raw failure,
`test_version_flag`, is one of those: the reference binary reports `compiled
22:13:51, Jun 11 2026`, while the hidden fixture expects `compiled 20:15:26,
Mar  5 2026`. The agent copied the only timestamp it could observe, and no
black-box implementation can match a fixture built from a different binary.
`programbench info` therefore scores 506/506 and marks the instance ✅, not
merely "almost".

The first attempt, `pb-cmatrix-20261110`, produced no score. The runner started
the task container with `sleep 2h`, the same duration as the agent budget; the
container exited at the moment the agent timed out, so the workspace was gone
before submission extraction. The stream shows the agent had written
`cmatrix.c` and `compile.sh`, built them, and was still comparing behaviour at
the two-hour mark (278 turns, 248 `Bash` calls) — an extraction failure, not a
model failure. The runner now starts the container with `sleep infinity`,
restarts a stopped container before reading the workspace, and gives the agent
a pacing instruction (first complete version early, last quarter for build and
checks). The pacing hint means the rerun is not a controlled repetition of the
first attempt, but neither attempt is a benchmark measurement.

This validates the whole path — cleanroom, allowlisted model access, headless
agent, submission extraction, official compile, 506 behavioural tests, and
`info` filtering. It is not a ProgramBench score: one easy instance, chosen
because it needs no downloaded model or dataset. `cmatrix` is a small, public,
well-known program, and the network allowlist stops lookup but not
memorization; treat this as a pipeline pass, not evidence of general
program-recovery ability.

### 2026-11-11 — three-instance sample: `figlet`, `pixterm`, `zip-password-finder` (**0/3 solved**)

- run-ids: `pb-cmatsuoka__figlet.202a0a8-20261110`,
  `pb-eliukblau__pixterm.1a93fd5-20261110`,
  `pb-agourlay__zip-password-finder.704700d-20261110`
- instances: C/easy, Go/easy, Rust/medium
- protocol: same as `cmatrix`; 7 200 s agent budget, 500 turns, 4 CPUs, 8 GB,
  internal network with only `opencode.ai:443` allowed; each 4+ GB cleanroom
  image removed after grading
- all three hit the 7 200 s timeout (exit 124); none finished early

| instance | turns | shell jobs | raw tests | official | outcome |
|---|---:|---:|---|---|---|
| `cmatsuoka__figlet.202a0a8` | 355 | 364 | 0/1 044 | 0, `ERROR: compile_failed` | final source did not compile |
| `eliukblau__pixterm.1a93fd5` | 53 | 53 | 276/458 | 60 (430 scored tests) | compiled; 172 scored tests failed |
| `agourlay__zip-password-finder.704700d` | 103 | 95 | 738/792 | 92 (680 scored tests) | compiled; 53 tests failed |

**`figlet` (C, easy).** The submission contains a reconstructed figlet tree and a
correct `compile.sh`, but `figlet.c:1518` has `error: 'else' without a previous
'if'`, so all 1 044 tests are `not_run`. The agent had built and self-tested
`myfig` around turn 340 (its own sweep reported no diffs on the main cases,
4 diffs on `ivrit`), then made about 20 more edits without recompiling. Two
`context window is full (128 429 / 129 449 estimated tokens)` aborts cost
3 456 s — 48 % of the budget. Only 63 s of the 7 200 s went to shell
execution: 364 jobs, median 0.01 s, while the 364 model gaps totalled 7 124 s
(median 14 s, p90 42 s).

**`pixterm` (Go, easy).** It compiles with `go build` (no module dependencies)
and passes 276/458 raw tests. The 172 scored failures split into 66 image
decodes (`unknown format`: 59 `.bmp`, 2 `.tiff`, 2 `.webp`), about 87 rendering
and dithering mismatches, about 20 CLI validation/exit-code differences, and 7
pytest timeouts. `go.mod` declares no dependencies, the module cache has no
`golang.org/x/image`, and the build phase is network-blocked, so supporting
those formats meant writing decoders; the agent repeated the README's format
list in `--help` but never implemented or tested them. Only 53 turns were made
in 2 hours: 8 s of shell time and 6 999 s in model gaps (median 74 s, p90
370 s, max 1 114 s), including one 653 s segment that ended on a transport error
(`error sending request for url .../v1/messages`). Most early turns probed the
reference's scaling and dithering with synthetic PNGs; the first source write
was call 36 of 58.

**`zip-password-finder` (Rust, medium).** It compiles with the cleanroom's
`rustc` and scores 738/792 raw; the official score is 92 on 680 scored tests,
**not solved** (no ✅; ProgramBench reserves that for 100). The 53 failures are
32 `Password not found`, 9 timeouts on larger search spaces, 7 exact
error-message mismatches around `--maxPasswordLen 0`, 3 directory-input cases,
1 `--` separator case, and 1 empty-charset-file case. The `not found` bucket is
not a crypto bug: the dictionary-attack tests largely pass, and the submitted
code's candidate enumeration is wrong. It walks one concatenated space of passwords of
increasing length, but when the per-worker stride carries past the last
password of a length, it puts the carry in the most-significant digit instead
of starting the next length at zero; the seeded value depends on the last digit
and the stride. With one worker that skips the first `n` candidates of each new
length; with multiple workers the digit/index mapping also diverges, so
candidates are both duplicated and skipped. For `-c l` (26 letters), `ab` is
length-2 candidate 1 and is skipped even with one worker; simulating the
submitted algorithm with 8 workers on the default 62-character alphabet tests
535 of 3 906 candidates up to length 2, and never tests `ab`. This instance is
also the only one of the three where tool time is substantial: 5 317 s across
95 jobs, against 6 000 s of model gaps (median 55 s, p90 151 s, max 593 s).

These are three hand-picked instances, one per language/difficulty, not a random
sample: 0/3 estimates nothing about ProgramBench as a whole. `caps-log` (C++,
medium) was deferred after the three runs above. The Claude Code harness control
on `figlet` and `pixterm` and the `caps-log` runs that turned into the
compaction investigation follow.

### 2026-11-11 — Claude Code harness control: `figlet`, `pixterm`

The same two instances were run with Claude Code 2.1.318 instead of Codape, to
separate "the model cannot recover this program" from "this harness's loop
cannot". Everything else was held equal: the same cleanroom image tag, the same
gateway/model (`deepseek-v4.1-flash` through `opencode.ai`), an internal network
that allows only the gateway, 4 CPUs, 8 GB, 500 turns and a 7 200 s agent
budget. `CLAUDE_CODE_MAX_CONTEXT_TOKENS=131072` matched the window Codape had.

- run-ids: `pb-claude-cmatsuoka__figlet.202a0a8-20261111`,
  `pb-claude-eliukblau__pixterm.1a93fd5-20261111`
- both were killed by the 7 200 s timeout (exit 124) and produced no closing
  `result` frame; the transcripts are stream-json frames only

| instance | assistant frames | tool calls | compactions | raw tests | official | outcome |
|---|---:|---|---:|---|---|---|
| `cmatsuoka__figlet.202a0a8` | 736 | 351 Bash, 20 Read, 19 Edit, 3 Write | 14 | 880/1 044 | 82 (872 scored) | compiled; 164 tests failed |
| `eliukblau__pixterm.1a93fd5` | 253 | 129 Bash | 8 | 0/458 | 0, `ERROR: compile_failed` | no `compile.sh` in the submission |

The control answers the question the Codape figlet run raised. Codape's two
`context window is full` aborts (128 429 and 129 449 estimated tokens against a
128 072 blocked line, the first only 357 over) were not an unavoidable property
of the task, the model, or the window: Claude Code compacted 14 times on the
same task, never aborted on context, and kept working until the clock ran out.
Codape's abort path was self-inflicted — an open circuit breaker refused every
pass before `Blocked` and then treated `Blocked` as fatal without one local
attempt — and the 3 456 s it spent in those aborts was 48 % of its budget.

The control is not a win for Claude Code. It also hit the 2-hour wall on both
instances, and on `pixterm` it did worse than Codape: its submission had no
`compile.sh` at all, so the harness could not build it (0/458, `compile_failed`),
while Codape's compiled and scored 60/430 official (276/458 raw). Eight
compactions in two hours is also a large share of a 253-frame run. What the
pair shows is narrower and useful: **a working compaction loop lets a run keep
going; a broken one converts a recoverable context into a terminal error.** The
compaction fixes recorded in `docs/compact.md` (trigger at `auto`, land at
`warn`, history-space targets, ladder through `LastResort`, maintenance passes
exempt from breaker accounting) target exactly that failure.

Both cleanroom images were removed after grading; no ProgramBench container or
image remains on the host.

### 2026-11-11 — `caps-log` with the default window guess: **0, `compile_failed`**

- run-id: `pb-nikoladucak__caps-log.2cf2d1e-20261111-r2`
- instance: `nikoladucak__caps-log.2cf2d1e` (C++, medium)
- protocol: as the three-instance sample, **except** that the runner did not
  yet pass the served window; Codape was left to guess
- agent: 393 assistant frames in 7 200 s (exit 124), 332 reported turns
- raw tests: 0/1 232 — the submission did not compile; official score 0,
  `ERROR: compile_failed`

The first attempt at this instance, `pb-nikoladucak__caps-log.2cf2d1e-20261111`,
is not a scored run. It ended in 2 s with four `error_during_execution` frames:
the gateway answered every request with `MissingSessionID` because the request
carried no `x-opencode-session` header. The retries below set it by forwarding
`CODAPE_CUSTOM_HEADERS` into the container, which the runner already supported.

The stream is a record of a compaction loop rather than of the task. It holds
241 `token_threshold` compaction events in 393 turns — 168 `compacted` and 73
`no_progress` — of which 238 ran the `LastResort` strategy, 2 `Snip`, and 1
`Auto`. The passes removed 433 018 estimated tokens in total but landed nowhere:
the median pass started at 9 636 estimated tokens, and the last passes left 0
messages in the history. No source file was written.

The arithmetic explains all of it. The runner served a 128k model but Codape
did not know the window, so it used `UNKNOWN_CONTEXT_WINDOW` (32 000). With
that value the compaction line is `32 000 − 20 000 − 13 000`, i.e. effectively
zero, so every request crossed it on the first turn; the landing line derived
from it is also zero, which no pass can reach, and progress fell back to "removed
at least a tenth". The only way to satisfy that is to delete history, and the
strategy ladder marched to `LastResort` within the first few passes.

Two estimator defects made the loop worse. `context_prefix_tokens` counted
messages only, so the serialized tool schemas — the largest fixed part of a
request — were invisible; on this gateway the measured prompt ran at about
1.95× the estimate, with roughly 8–9k tokens of fixed overhead. And the targets
subtracted only the message prefix (1 290 in the first pass above), asking the
history to shed room that the tool schemas had already spent. Every pass then
looked successful by the tenth-removed rule while the real request stayed over
the real limits.

The fixes are in `docs/compact.md` and in the commit that follows this run: the
runner now passes the served window to Codape via `CODAPE_CONTEXT_WINDOW`, the
request estimate includes the tool schemas, a measured request records its
fixed overhead and targets subtract it, an accepted request raises and persists
the window's lower bound, a threshold pass that cannot reach the landing line
returns `Uncompactable`, and a pass that removed nothing is not retried at the
same size. The reruns that exercise these are recorded below.

### 2026-11-11 — `caps-log` with the served window passed in: **71, not solved**

- run-id: `pb-nikoladucak__caps-log.2cf2d1e-20261111-r3`
- instance: `nikoladucak__caps-log.2cf2d1e` (C++, medium)
- protocol: as r2, with `CODAPE_CONTEXT_WINDOW=131072` exported into the
  container by hand (the runner would learn to do this itself before r4)
- agent: 146 assistant frames, 135 reported turns, 2 750 s, exit 0 and a
  `success` result frame — it finished on its own instead of hitting the clock
- tools: 135 `Bash`, 7 `Write`, 3 `Edit`, 1 `Read`; the submission compiles
- raw tests: 717/1 232; official score 71

The window override alone changed the run from a compaction loop into a task.
There were 4 `token_threshold` compactions, all `Auto`, starting at 84 993–
89 787 estimated tokens — the line the 128k window actually implies — against
241 passes (238 of them `LastResort`) in r2. No pass was retried after removing
nothing, no turn aborted on context, and the agent spent its time writing
`source/*.cpp`, probing the reference, and running the cleanroom's build.

ProgramBench's official score filters the report: 681 of the 1 232 reported
tests are marked ignored in `tests.json` (two of the twelve branches,
`78a25e6570f4` and `c60a1c23d5ae`, are ignored in their entirety; the others
lose individual `dummy_pass`/`gold_fail` entries). That leaves 551 scored
tests, 390 of them passed — 71. The failures cluster in configuration
validation (36 in `tests.test_config`, 15 in `test_config_error_paths`, 8 in
`test_cli_options`), git integration and operations (15 + 15 + 9 in
`test_log_repository_errors`), and smaller groups in annual-log data,
encryption, scratchpad, and app lifecycle. These are fidelity gaps in the
reconstruction, not loop failures: the agent reached the end of the task with
time to spare and simply did not implement the config parser's error surface
and git paths closely enough.

One branch, `c0e6b8d64108` (3 config/env tests, the only remaining branch that
launches the TUI and expects it to time out), produced no `results.xml` after
two attempts and consumed about 31.5 of the eval's 33.3 minutes. The
three tests are reported `not_run` and count against the score. The cause was
not established from the log; the shape — a submitted TUI process that cannot
be torn down while the test holds its captured pipes — makes it a harness
interaction worth isolating before the next run, not a reason to distrust the
71. The three tests are 0.5 % of the scored total.

### 2026-11-11 — `caps-log` on the fixed estimator, before the target work: **72, timed out**

- run-id: `pb-nikoladucak__caps-log.2cf2d1e-20261111-r4`
- instance: `nikoladucak__caps-log.2cf2d1e` (C++, medium)
- protocol: as r3, except that the runner now exports
  `CODAPE_CONTEXT_WINDOW=131072` into the container itself and the session
  header is minted per run
- agent: 241 reported turns, 247 assistant frames, 7 199 995 ms, exit 124 —
  `SIGTERM` ended the run while a turn was in flight
- tools: the agent was still working at the cutoff (its last commands read the
  reference `transcript.txt` and probe `--log-filename-format`), not looping
- raw tests: 682/1 232 passed (522 failed, 25 skipped, 3 not run); official
  score **72**
- eval: 17 min, one instance; branch `c0e6b8d64108` again produced no
  `results.xml` (`results_read_failed`, its 3 tests `not_run`)

The binary in this run is the one built from the estimator fixes r3's section
describes (served window, tool schemas, measured fixed overhead, learned
window lower bound, no-progress backoff). It predates the observability and
target commits that came after it: the `summary` label on the event, the
summary tail sized from the landing target instead of `window / 8`, and
Micro/Auto stopping once under target. It is the baseline the next run is
compared against.

There were 6 `token_threshold` compactions, all `Auto`, each landing far
*below* its target:

| `tokens_before` | `tokens_after` | `target_tokens` | `messages_remaining` |
|---|---|---|---|
| 88 388 | 45 564 | 69 931 | 63 |
| 105 863 | 40 036 | 78 072 | 15 |
| 94 888 | 15 760 | 75 090 | 22 |
| 86 984 | 20 820 | 68 122 | 26 |
| 86 781 | 23 475 | 68 816 | 29 |
| 71 053 | 29 421 | 55 509 | 24 |

Every pass removed 42–79 K estimated tokens when meeting the target required
12–60 K, so each one discarded context the summary path could have kept. This
is the overshoot the rule-based ladder had by design: it truncated every
eligible tool result and removed half of the older assistant texts regardless
of how much room was needed. The event predates the `summary` field, so it
cannot say whether a given pass ran the summary path or the rule path; the
landing points are consistent with the rule path, and the next run adds the
label so the two are distinguishable.

The official score filters the same ignored tests as r3 (681 of the 1 232
reported), so it is computed over the same 551 scored tests. The run did not
finish, and its raw pass count (682) is 35 lower than r3's (717), while the
official score is one point higher (72 vs 71) — the filtered score is within
run-to-run variance, and the difference in duration is not a compaction
failure. r3 ended in 2 750 s after 135 turns; r4 spent the full
two hours and 241 turns probing the reference implementation. The same
`results_read_failed` branch error as r3 recurred and again cost its 3 tests.

### 2026-11-12 — `caps-log` with the target-sized summary tail: **89, not solved**

- run-id: `pb-nikoladucak__caps-log.2cf2d1e-20261111-r5`
- instance: `nikoladucak__caps-log.2cf2d1e` (C++, medium)
- binary: built from the commit that sizes the summary tail from the landing
  target and labels the summary path on the event; the Micro/Auto stop-at-target
  change is *not* in this build
- protocol: as r4 (runner exports `CODAPE_CONTEXT_WINDOW=131072`, fresh session
  header, 4 CPUs, 8 GB, 7 200 s, 500 turns)
- agent: 157 reported turns, 160 assistant frames, 4 603 488 ms, exit 0 and a
  `success` result frame — it finished with about 43 minutes unused
- submission: sources, `compile.sh` and `executable` placed at the workspace
  root and committed
- raw tests: 775/1 232 passed (429 failed, 25 skipped, 3 not run); official
  score **89** (490 of the same 551 scored tests)
- eval: the same `results_read_failed` branch, `c0e6b8d64108` (3 `not_run`)

Seven compaction-related events, all `token_threshold`:

| kind | strategy | summary | before | after | target |
|---|---|---|---|---|---|
| compacted | Auto | summarized | 72 800 | 39 207 | 57 907 |
| compacted | Auto | summarized | 78 941 | 49 413 | 62 449 |
| no_progress | Auto | — | 82 883 | 82 200 | — |
| backed_off ×3 | — | — | 100 689 / 102 124 / 104 015 | — | retry at ~104 K |
| compacted | Snip | empty | 87 032 | 54 938 | 69 997 |

Two changes are directly visible. First, the summary path ran and is now
labelled: r4 reported `effective: Auto` with no way to tell a summary from a
rule pass; r5 says `summarized`. Those passes kept a target-sized tail —
39 207 against a 57 907 target, then 49 413 against 62 449 — where r4's rule
passes landed 15 760–45 564 against 55 509–78 072. The first landing is the
tail rule doing what it is designed to do (two thirds of the target kept
verbatim, the rest elided into the summary); the second is within 21 % of its
target, against r4's 24–60 K undershoot.

Second, the no-progress machinery held. The third Auto pass had little left to
remove (82 883 → 82 200) because the summary had already elided the bulk of
the history; instead of retrying the same pass, the engine emitted three
`backed_off` events and waited for the context to grow, then a `Snip` pass with
an empty summary recovered 32 K to 54 938. No pass was repeated at the same
size, and the run kept moving.

The score rose from r3's 71 and r4's 72 to 89, with raw passes up from 717 and
682 to 775. Against r4 the run gained 105 tests and lost 12; the newly passing
tests are concentrated in the clusters r3's section named as fidelity gaps —
`test_config` (25), `test_log_files` (17), `test_config_error_paths` (14),
`test_git_integration` (12). The trajectory is not a controlled repeat: r5
finished on its own and had time to place and commit the deliverables at the
workspace root, while r4 was cut off mid-turn. The compaction events above are
the direct evidence that the target-sized path behaved differently; the score
difference is consistent with that but also includes run-to-run agent variance.

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
| [CodeClash](https://codeclash.ai/) | Goal-oriented software engineering rather than tasks: agents start from a high-level objective and evolve a codebase over rounds, scored by relative competition in arenas (Halite, Poker, CoreWar, RobotRumble, Robocode, BattleSnake). Ranked by Elo. | 6 arenas | Own arenas and multi-round protocol; an agent is run repeatedly as a competitor, not once per instance |

**SWE-rebench v2** is the one worth more than the rest here: it is the only
benchmark in this table that makes a contamination story plausible, and its
result is a percentage at a *dated window*, which is a stronger claim than a
fixed-split number.

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
  online, and ProgramBench's runner enforces that rule with a network allowlist
  rather than prompt text; a memorized fix is still reproducible. Treat scores
  as upper bounds.
- **Whose failure it was.** A benchmark score of 0 can mean the agent harness
  broke, or the model declined the task. [terminal-bench.md](terminal-bench.md)
  exists mostly to keep the frame subtype and the test's own verdict apart —
  the same discipline applies to any runner added here.

Cost is unbounded by default. Bound it with `--max-turns` and `--timeout`, use a
cheap model for plumbing checks, and estimate spend from the provider's billing
dashboard: Codape 0.2.0 emits a `result` frame but does not populate `usage`, so
the runners print `tokens/cost: unavailable` rather than inventing numbers.
