# Codape vs Claude Code on Terminal-Bench 4.1, same model

A controlled comparison: identical dataset, identical model
(`deepseek-v4.1-flash`), identical gateway, identical task list, identical
`max_turns=300`. The only variable is the agent. Harbor's built-in `claude-code`
agent is the reference — it is the implementation this project is being
optimised toward, so a gap against it is a gap against the target, not against
an arbitrary baseline.

Two jobs on `ubuntu@161.120.163.150` (4 vCPU, 15 GB):

| arm | job | notes |
|---|---|---|
| Codape | `2026-11-24__23-33-13` | run **before** the model table and context-window fixes |
| Claude Code | `2026-11-25__06-47-04` | Harbor's `claude-code` agent, same nine tasks |

**The Codape arm ran on a 32 K window with a 16 K compaction line**, because
`deepseek-v4` had no entry in the model table (fixed in `ee448320`). That is a
real handicap and it is not symmetric: the Claude arm ran with whatever window
Harbor's client resolved. Read the numbers below with that in mind — the
per-turn overhead it caused is quantified at the end.

## The result

| task | Codape | Claude Code | verdict |
|---|---|---|---|
| `react-lead-form` | **1.0** 167/167, 43 min | **1.0** 167/167, 15 min | tie on score, Codape 2.8× slower |
| `kv-live-surgery` | 0.0 4/6, 118 min, 300 turns | **1.0** 6/6, 34 min | **Claude solves it, Codape does not** |
| `vf2-speedup-networkx` | 0.0 **57/60**, 133 min | 0.0 54/60, 65 min | Codape ahead by 3 tests |
| `gsea-proteomics` | 0.0 10/16, 16 min | 0.0 11/16, 16 min | Claude +1 test |
| `vllm-deepseek-streaming` | 0.0 3/7, 13 min | 0.0 3/7, **1 min** | identical tests, Codape 13× slower |
| `ks-solver-cpp` | 0.0 0/2, 20 min | 0.0 1/2, 83 min | Claude +1 test; Codape emitted no answer |
| `nextjs-performance` | 0.0 2/7, 44 min, 300 turns | 0.0 2/7, 24 min | tie on tests, Codape 1.8× slower |
| `biped-contact-dynamics` | 0.0 0/3, 17 min | 0.0 2/3, 89 min | Claude +2 tests; Codape emitted no answer |
| `foodstuff-beta-activity` | 0.0 10/13, 13 min | 0.0 10/13, 23 min | tie on tests, Codape faster |

**Codape 1, Claude 4, tie 4** across the nine. Terminal-Bench 4.1 is
**all-or-nothing**: `reward` is 1.0 or 0.0, so only two of nine are "solved" by
either arm and the per-test counts carry most of the signal.

The directions still disagree, which is what makes the setup usable: Codape
wins `vf2` by three tests and is faster on `foodstuff`, loses `kv-live-surgery`
outright and is 2–3× slower on four others. Nine tasks is too few to rank the
two agents; it is enough to locate specific defects, which is what this file is
for.

The two zero-output tasks are the sharpest single finding. On both,
**Codape produced no answer at all** while Claude produced a partial one — the
same failure the model-table entry fixes, and the reason the analysis below is
about ceilings rather than about reasoning quality.

## The finding: output is ~98 % reasoning

Across the whole Codape arm, **97.96 % of all written output is
`reasoning_content`** — thinking blocks — and 2.04 % is the answer a task
actually consumes. Per task:

| task | reasoning chars | answer chars | reasoning share |
|---|---|---|---|
| `kv-live-surgery` | 1 768 425 | 4 357 | **99.8 %** |
| `vf2-speedup-networkx` | 2 080 063 | 3 206 | **99.8 %** |
| `nextjs-performance` | 449 285 | 1 788 | **99.6 %** |
| `ks-solver-cpp` | 439 464 | 3 170 | **99.3 %** |
| `biped-contact-dynamics` | 371 090 | 5 580 | 98.5 % |
| `react-lead-form` | 335 029 | 6 502 | 98.1 % |
| `foodstuff-beta-activity` | 219 984 | 4 050 | 98.2 % |
| `gsea-proteomics` | 65 454 | 5 422 | 92.4 % |
| `vllm-deepseek-streaming` | 19 215 | 85 425 | 18.4 % |
**Claude Code does the same thing.** Its trajectory is 97–99 % reasoning on
every task where the split is available. So this is a property of the model on
this benchmark, not something Codape adds. It does rule out the whole family of
harness-side optimisations aimed at the answer: trimming the answer, shortening
tool output, or trimming the prompt prefix all act on the 2 %.

### And the harness never asked for it

The obvious next lever would be the thinking budget — Codape has `--effort`,
which maps to `thinking.budget_tokens`. **Neither arm passes it.** The Codape
adapter's command line is `codape -p ... --max-turns 300`, with no `--effort`,
so `effective_reasoning_effort()` is `None`, `apply_reasoning_effort` returns
on its first line, and the request body carries **no `thinking` field at all**.

Probing the endpoint with exactly that body — model, `max_tokens`, one user
message, nothing else — still returns a `thinking` block followed by text. So
the 98 % is the model's own default on this gateway, not something either
harness requested. There is no harness-side knob to turn here: the only way to
make the model think less is a parameter this endpoint does not require the
caller to send and that neither agent sends.

Two consequences follow, and they cut against the obvious plan:

- Trimming, capping, or budgeting the reasoning is **not** available as a
  harness optimisation on this model. That rules out the one lever the
  measurement seemed to leave open.
- The remaining lever is what it does with the thinking it produced — turning
  it into a tool call before the ceiling arrives. That is the `error_no_output`
  defect below, and `retry_ceiling` is the mechanism already in the code.

**A related gap that is real but did not affect these numbers.** Codape
resolves its provider profile from the base URL, and `opencode.ai` matches
neither `anthropic.com` nor `openai.com`, so it resolves to `custom`, whose
`reasoning_effort_support()` is `Unsupported`. Both arms ran with `--effort`
unset, so nothing was lost here — but a user who *did* set `--effort` on this
gateway would have it silently dropped with a warning, even though the gateway
accepts both the `budget_tokens` and the `output_config.effort` encodings
(probed: both return 200). `CODAPE_PROVIDER_PROFILE=anthropic` is the
workaround; it is not set by anything in this repo. Not changed while the sweep
runs — altering request parameters mid-sweep would make the two halves of the
Codape arm incomparable.

## The defect the comparison finds

`ks-solver-cpp` is the clean case. Codape spent **four consecutive turns at
~110 000 reasoning characters each, emitting no text and no tool call**, then
stopped with `subtype: error_no_output`. 131 196 output tokens, zero answer,
0/2 tests.

That is `TokenLimitStop { produced: false }` — the turn hit `max_tokens` with
nothing to show, which is the reasoning model spending its entire ceiling
thinking. The trajectory shows it directly: four model steps, one with two tool
calls, then **three consecutive steps of ~110 000 reasoning characters with zero
message and zero tool calls**.

The usage field says the same thing independently. `ks-solver-cpp` billed
**131 196** output tokens — 124 above `4 × 32 768`, so four turns each ending
exactly on the 32 768 default ceiling. The arithmetic rules out a retry having
fired: one turn at 65 536 plus its 32 768 attempt is 98 304 by itself, and three
empty turns would put the total above 163 840. So `retry_ceiling()` returned
`None` on every one of them. It does that when the context window is **guessed**
(`window.source.is_guess()`), refusing to raise a ceiling against a window it
does not know. A guessed 32 K window also put the compaction line at 16 K, so
the session compacted every few turns and lost history.

Both causes are the same missing table entry, fixed in `ee448320`:

```rust
("deepseek-v4.1", 1_000_000),
("deepseek-v4",   1_000_000),
```

With the entry present, `retry_ceiling` computes `32_768 × 2 = 65_536` against
1 M of room and the retry fires — and the gateway accepts that ceiling, which
is not something to assume. Probed directly against the same endpoint with the
same credential, `max_tokens` 32 768 and 65 536 both return `200` with
`stop_reason: end_turn`; the model entry's 32 768 is the *default* this build
asks for, not a hard rejection limit at the gateway. Without that check, a
retry the code considers legal could have turned every empty-answer turn into a
400 instead of recovering it.

`biped-contact-dynamics` ended the same way (12 turns, 100 576 output tokens,
0/3). Claude, on the same two tasks, produced a partial answer at 2–3× the
token spend — 385 437 tokens for 2/3 on `biped`, 286 912 for 1/2 on
`ks-solver` — and its reasoning share was just as high (99 %). **The gap is not
thinking too much; it is failing to convert thinking into action when the
ceiling arrives.** Claude kept acting under the same ceiling; Codape thought
until the budget was gone.

## Cost

Codape reports no cost for these runs (its adapter does not price the model).
Across all nine tasks Codape spent **1 907 502** output tokens to Claude's
**1 645 547** — 1.16× for one solved task against two.

The per-task ratio is where the shape shows:

| task | Codape out | Claude out | ratio |
|---|---|---|---|
| `kv-live-surgery` | 575 234 | 179 098 | **3.2×** |
| `vf2-speedup-networkx` | 674 732 | 331 914 | **2.0×** |
| `nextjs-performance` | 158 081 | 99 128 | 1.6× |
| `ks-solver-cpp` | 131 196 | 286 912 | 0.5× |
| `biped-contact-dynamics` | 100 576 | 385 437 | **0.3×** |
| `react-lead-form` | 120 888 | 120 128 | 1.0× |

Codape overspends by 2–3× where it *fails slowly* (`kv-live-surgery`,
`vf2` — 300 turns each), and underspends 3× where it *fails fast*
(`biped`, `ks-solver` — both zero-output). That is one defect, not two:
those two tasks spent the budget on thinking and emitted nothing, so the
token count is low precisely because the run collapsed.

The `vf2` case is the extreme input-side split: Codape 9.7 M input / 674 732
output for 57/60; Claude 47.8 M input / 331 914 output for 54/60. Codape sent
**one fifth the prompt** and wrote **twice the answer tokens** — the opposite
of the caching story one would expect from the prefix work.

## A caveat that must be read with every number here

**The Codape arm ran `v0.2.2`, a published release, not the working tree.** Its
`agent_result.metadata.version` records `v0.2.2`, and the adapter's
`DEFAULT_VERSION` is what installed it. `ee448320` — the model-table fix this
whole analysis points at — is *not* in that release, so the Codape column above
measures the bug, deliberately and on the record.

The sweep that follows it compensates at the boundary instead: `run-arm.sh`
exports `CODAPE_CONTEXT_WINDOW=1000000`, which v0.2.2 resolves as an `Env`
source, and `is_guess()` is false for `Env`. So `retry_ceiling` gets a known
window and the retry can fire without the table entry. Verified by reading
v0.2.2's own `resolve_detailed` and `is_guess`. What the env override does
**not** fix is the output-cap half: v0.2.2 has no `deepseek-v4` entry in
`MODEL_MAX_OUTPUT_TOKENS`, so the compaction reserve is the flat 20 000 rather
than the model's 32 768 — but `min(20_000, 32_768)` is 20 000, so the resolved
compaction line is identical either way. The two paths agree for this model,
and that is why the sweep remains comparable to the control despite the version
difference.


## What the trajectory artifact was not recording

The Codape arm's `trajectory.json` recorded **2 steps for a 300-turn run** —
`vf2` shows `total_steps: 2` with 306 tool calls folded into one step. The
adapter flushed its accumulated turn only on `result`, and Codape emits
`result` once per *session*, not per turn. Fixed in `2398f143`; the Claude arm
records 75–192 steps for the same runs, which is the shape the artifact should
have.

This matters for the four-layer plan specifically: the per-turn record is the
input to any turn-level cost analysis, and it was empty.

## Reproducing

```
python3 -I scripts/external-benchmarks/terminal-bench/analyze_sweep.py \
    <codape-job-dir> <claude-job-dir>
```

Every number above is read from `result.json` (`verifier_result.rewards`, the
`agent_result` token fields, `agent_execution` timestamps) and from
`verifier/**/ctrf.json` and `agent/trajectory.json`. Nothing is parsed out of
logs by substring.
