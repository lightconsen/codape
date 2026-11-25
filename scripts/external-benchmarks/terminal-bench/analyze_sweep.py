#!/usr/bin/env python3
"""Compare the two arms of the Terminal-Bench sweep, task by task.

Reads two harbor job directories and reports, per task, what each arm scored,
how long it took, what it cost in tokens, and — the thing the four-layer
optimisation plan actually turns on — how much of each arm's output was
reasoning rather than answer.

    python3 -I analyze_sweep.py <codape-job-dir> <claude-job-dir>

Every field read here is one harbor writes for every trial, so a job from an
older adapter still compares on the columns that exist. A trial absent from one
arm is reported as such rather than scored zero: a task that never ran and a
task that ran and failed are different facts.

Scores are all-or-nothing in Terminal-Bench 4.1 — `reward` is 0.0 or 1.0 — so
the per-test ctrf count is carried alongside it. Two arms can both score 0.0
with 57/60 and 54/60 tests passing, and that gap is the only signal a
zero-reward task carries.
"""

from __future__ import annotations

import datetime
import glob
import json
import os
import sys


def load_trial(directory: str) -> dict:
    """Everything this comparison needs from one trial directory."""
    name = os.path.basename(directory.rstrip("/")).split("__")[0]
    result_path = os.path.join(directory, "result.json")
    trial = {"name": name, "ran": False}

    if not os.path.exists(result_path):
        return trial
    trial["ran"] = True
    try:
        with open(result_path, encoding="utf-8") as handle:
            result = json.load(handle)
    except (OSError, json.JSONDecodeError):
        return trial

    rewards = (result.get("verifier_result") or {}).get("rewards") or {}
    trial["reward"] = rewards.get("reward")
    trial["reward_valid"] = result.get("reward_valid")

    agent = result.get("agent_result") or {}
    trial["output_tokens"] = agent.get("n_output_tokens")
    trial["input_tokens"] = agent.get("n_input_tokens")
    trial["cache_tokens"] = agent.get("n_cache_tokens")
    trial["cost_usd"] = agent.get("cost_usd")

    metadata = agent.get("metadata") or {}
    trial["subtype"] = metadata.get("subtype")
    trial["num_turns"] = metadata.get("num_turns")
    # `model_ms` is time waiting on the model and `tool_ms` time running tools.
    # Their ratio is what says whether a slow task was slow in the agent or in
    # the model, and only the latter is worth optimising in the harness.
    trial["model_ms"] = metadata.get("model_ms")
    trial["tool_ms"] = metadata.get("tool_ms")
    trial["duration_ms"] = metadata.get("duration_ms")

    execution = result.get("agent_execution") or {}
    trial["minutes"] = _minutes(execution.get("started_at"), execution.get("finished_at"))

    trial["passed"], trial["tests"] = _ctrf(directory)
    reasoning, answer = _text_volume(directory)
    trial["reasoning_chars"] = reasoning
    trial["answer_chars"] = answer
    return trial


def _minutes(start: str | None, end: str | None) -> float | None:
    def parse(value: str | None) -> datetime.datetime | None:
        if not value:
            return None
        try:
            return datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
        except ValueError:
            return None

    first, last = parse(start), parse(end)
    if first is None or last is None:
        return None
    return (last - first).total_seconds() / 60


def _ctrf(directory: str) -> tuple[int | None, int | None]:
    """The verifier's own pass/total counts, which a 0.0 reward still carries."""
    for path in glob.glob(os.path.join(directory, "**", "ctrf.json"), recursive=True):
        try:
            with open(path, encoding="utf-8") as handle:
                summary = json.load(handle).get("results", {}).get("summary", {})
        except (OSError, json.JSONDecodeError):
            continue
        return summary.get("passed"), summary.get("tests")
    return None, None


def _text_volume(directory: str) -> tuple[int, int]:
    """Characters of reasoning and of answer in the ATIF trajectory.

    The split is the point. A run whose output is 98 % reasoning is paying
    completion-token prices for thinking, and the visible answer — the only
    part the task consumes — is a rounding error beside it.

    Returns zeros when there is no trajectory: an older job, or a run that
    died before writing one.
    """
    path = os.path.join(directory, "agent", "trajectory.json")
    if not os.path.exists(path):
        return 0, 0
    try:
        with open(path, encoding="utf-8") as handle:
            trajectory = json.load(handle)
    except (OSError, json.JSONDecodeError):
        return 0, 0
    reasoning = answer = 0
    for step in trajectory.get("steps") or []:
        reasoning += len(step.get("reasoning_content") or "")
        answer += len(step.get("message") or "")
    return reasoning, answer


def load_arm(job_directory: str) -> dict[str, dict]:
    trials = {}
    for directory in sorted(glob.glob(os.path.join(job_directory, "*/"))):
        trial = load_trial(directory)
        trials[trial["name"]] = trial
    return trials


def _reward(value: float | None) -> str:
    if value is None:
        return "-"
    return "1.0" if value == 1.0 else ("0.0" if value == 0.0 else f"{value:.2f}")


def _counts(passed: int | None, tests: int | None) -> str:
    if passed is None:
        return "-"
    return f"{passed}/{tests}"


def _minutes_text(minutes: float | None) -> str:
    return "-" if minutes is None else f"{minutes:.0f}m"


def _reasoning_share(trial: dict) -> str:
    reasoning = trial.get("reasoning_chars") or 0
    answer = trial.get("answer_chars") or 0
    if not (reasoning + answer):
        return "-"
    return f"{reasoning / (reasoning + answer) * 100:.0f}%"


def report(codape: dict[str, dict], claude: dict[str, dict]) -> None:
    names = sorted(set(codape) | set(claude))
    header = (
        f"{'task':<32} {'codape':>20} {'claude':>20}  {'delta':>7}"
    )
    print(header)
    print("-" * len(header))

    wins = {"codape": 0, "claude": 0, "tie": 0, "incomplete": 0}
    score_rows = []
    for name in names:
        left = codape.get(name) or {"ran": False}
        right = claude.get(name) or {"ran": False}

        def cell(trial: dict) -> str:
            if not trial.get("ran"):
                return "not run"
            return (
                f"{_reward(trial.get('reward'))} "
                f"{_counts(trial.get('passed'), trial.get('tests'))} "
                f"{_minutes_text(trial.get('minutes'))}"
            )

        delta = "-"
        if left.get("ran") and right.get("ran"):
            lp = left.get("passed") or 0
            rp = right.get("passed") or 0
            lr = left.get("reward") or 0.0
            rr = right.get("reward") or 0.0
            if lr != rr:
                wins["codape" if lr > rr else "claude"] += 1
                delta = "codape" if lr > rr else "claude"
            elif lp != rp:
                # Same all-or-nothing reward; the verifier's own count is the
                # only thing that separates them.
                wins["codape" if lp > rp else "claude"] += 1
                delta = f"{lp - rp:+d} tests"
            else:
                wins["tie"] += 1
                delta = "tie"
            score_rows.append((name, left, right))
        else:
            wins["incomplete"] += 1

        print(f"{name:<32} {cell(left):>20} {cell(right):>20}  {delta:>7}")

    print()
    print(
        f"codape {wins['codape']}  claude {wins['claude']}  "
        f"tie {wins['tie']}  one-arm-only {wins['incomplete']}"
    )

    _totals("codape", codape)
    _totals("claude", claude)
    _reasoning_table(score_rows)


def _totals(label: str, trials: dict[str, dict]) -> None:
    ran = [t for t in trials.values() if t.get("ran")]
    if not ran:
        print(f"\n{label}: no trials")
        return
    scored = [t for t in ran if t.get("reward") == 1.0]
    minutes = [t["minutes"] for t in ran if t.get("minutes") is not None]
    output = [t["output_tokens"] for t in ran if t.get("output_tokens")]
    print(
        f"\n{label}: {len(ran)} trials, {len(scored)} solved "
        f"({len(scored) / len(ran) * 100:.0f}%), "
        f"{sum(minutes):.0f} min total"
        + (f", {sum(output):,} output tokens" if output else "")
    )


def _reasoning_table(rows: list[tuple[str, dict, dict]]) -> None:
    """Where the output tokens went, per arm, on the tasks both arms ran.

    This is the four-layer plan's central question: if the answer is a few
    thousand characters and the reasoning a few hundred thousand, then the
    harness's own overheads — prompt prefix, compaction, tool round trips —
    are noise next to how much the model thinks, and no amount of prefix
    trimming changes the bill.
    """
    usable = [
        (name, left, right)
        for name, left, right in rows
        if (left.get("reasoning_chars") or left.get("answer_chars"))
        and (right.get("reasoning_chars") or right.get("answer_chars"))
    ]
    if not usable:
        return
    print()
    print(f"{'task':<32} {'codape out':>11} {'codape rsn':>10}  "
          f"{'claude out':>11} {'claude rsn':>10}")
    print("-" * 78)
    total_c = total_cl = 0
    for name, left, right in usable:
        print(
            f"{name:<32} {left.get('output_tokens') or 0:>11,} "
            f"{_reasoning_share(left):>10}  "
            f"{right.get('output_tokens') or 0:>11,} {_reasoning_share(right):>10}"
        )
        total_c += left.get("output_tokens") or 0
        total_cl += right.get("output_tokens") or 0
    print("-" * 78)
    print(f"{'TOTAL':<32} {total_c:>11,} {'':>10}  {total_cl:>11,}")


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        print("usage: analyze_sweep.py <codape-job-dir> <claude-job-dir>", file=sys.stderr)
        return 2
    codape_dir, claude_dir = sys.argv[1], sys.argv[2]
    for directory in (codape_dir, claude_dir):
        if not os.path.isdir(directory):
            print(f"not a directory: {directory}", file=sys.stderr)
            return 1
    report(load_arm(codape_dir), load_arm(claude_dir))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
