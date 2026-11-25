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
| `ks-solver-cpp` | 0.0 0/2, 20 min | 0.0 1/2 | Claude +1 test; Codape emitted no answer |
| `nextjs-performance` | 0.0 2/7, 44 min, 300 turns | — | |
| `biped-contact-dynamics` | 0.0 0/3, 17 min | — | Codape emitted no answer |
| `foodstuff-beta-activity` | 0.0 10/13, 13 min | — | |

Terminal-Bench 4.1 is **all-or-nothing**: `reward` is 1.0 or 0.0, so only one
task out of nine is "solved" by either arm. The per-test counts are the real
signal and they are what the verdict column reads.

The directions disagree — Codape wins on `vf2`, loses on `kv-live-surgery` — so
there is no one-way systematic bias in the setup. Nine tasks is too few to rank
the two agents; it is enough to locate specific defects, which is what this
file is for.

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

What it does *not* rule out is how much thinking the harness asks for. Codape
requests a thinking budget; the size of that budget is a harness decision.

## The defect the comparison finds

`ks-solver-cpp` is the clean case. Codape spent **four consecutive turns at
~110 000 reasoning characters each, emitting no text and no tool call**, then
stopped with `subtype: error_no_output`. 131 196 output tokens, zero answer,
0/2 tests.

That is `TokenLimitStop { produced: false }` — the turn hit `max_tokens` with
nothing to show, which is the reasoning model spending its entire ceiling
thinking. The arithmetic in the usage field says the same thing independently:
`ks-solver-cpp` billed **131 196** output tokens, which is `4 × 32 768 + 124` —
four turns that each ended exactly on the 32 768 default ceiling, with no
65 536 retry. `retry_ceiling()` exists to recover from exactly this by
re-asking with a larger ceiling, and it returned `None` because the context
window was **guessed** (`window.source.is_guess()`), and it refuses to raise a
ceiling against a window it does not know. A guessed 32 K window also put the
compaction line at 16 K, so the session compacted every few turns and lost
history.

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
0/3). Claude, on the same task family, took 106 steps and scored 1/2 on
`ks-solver-cpp` — it also produced 736 298 characters of reasoning, but it kept
acting. **The gap is not thinking too much; it is failing to convert thinking
into action when the ceiling arrives.**

## Cost

Codape reports no cost for these runs (its adapter does not price the model).
Claude Code's arm cost **$60.50** for five tasks. On the four tasks both arms
ran, Codape spent 1 440 721 output tokens to Claude's 697 821 — **2.1× the
output for the same or worse score**.

The `vf2` case is the extreme: Codape 9.7 M input / 674 732 output for 57/60;
Claude 47.8 M input / 331 914 output for 54/60. Codape sent **one fifth the
prompt** and wrote **twice the answer tokens**.

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
