#!/usr/bin/env python3
"""Compare the two arms of the Terminal-Bench sweep, task by task.

Reads harbor job directories and reports, per task, what each arm scored, how
long it took, what it cost in tokens, and — the thing the four-layer
optimisation plan actually turns on — how much of each arm's output was
reasoning rather than answer.

    python3 -I analyze_sweep.py <job-dir>...

An arm is not one job. The sweep runs each arm as a concurrent batch plus a
separate single-core batch for the tasks that claim 16 GB, and the control
tasks may have been run earlier still, so one arm arrives as several job
directories. They are merged here, and the arm each belongs to is read from the
job's own `config.json` rather than from the order the directories were given.

Every field read here is one harbor writes for every trial, so a job from an
older adapter still compares on the columns that exist. A trial absent from one
arm is reported as such rather than scored zero: a task that never ran and a
task that ran and failed are different facts. A trial whose directory exists
but whose `result.json` has not been written yet is reported as still running,
which is what makes this usable while a sweep is halfway through.

Scores are all-or-nothing in Terminal-Bench 4.1 — `reward` is 0.0 or 1.0 — so
the per-test ctrf count is carried alongside it. Two arms can both score 0.0
with 57/60 and 54/60 tests passing, and that gap is the only signal a
zero-reward task carries.

A trial that never reached the model is **not a loss**. Harbor grades a quota
or setup failure like any other trial, so `reward` is a real 0.0 for a run that
spent no tokens — and counting those as losses is how this tool first reported
`codape 26, claude 3` on a sweep whose honest answer was a tie. A trial with
zero input and zero output tokens is reported as `no model`; a trial that did
reach the model but was cut off by an error is reported with its exception, and
neither is scored against the other arm.
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
    # A directory without a result is not the same as a directory that is not
    # there: the first is a trial in flight, the second a task this arm never
    # ran. Reporting both as "not run" would make a half-finished sweep look
    # like a task was dropped.
    trial = {"name": name, "ran": False, "present": os.path.isdir(directory)}

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
    # `result_frames` is how many times the agent emitted a session-ending
    # frame. One is a normal run. More than one means the harness drove the
    # session again after it ended — and since the frame that ends a run is
    # `error_during_execution` far more often than not, the count is the only
    # surviving evidence, in a finished trial, that a run was interrupted and
    # redone. The raw stream that would show why is deleted when the trial
    # closes; `trajectory.json` keeps the steps but not the failure.
    trial["result_frames"] = metadata.get("result_frames")
    # `model_ms` is time waiting on the model and `tool_ms` time running tools.
    # Their ratio is what says whether a slow task was slow in the agent or in
    # the model, and only the latter is worth optimising in the harness.
    trial["model_ms"] = metadata.get("model_ms")
    trial["tool_ms"] = metadata.get("tool_ms")
    trial["duration_ms"] = metadata.get("duration_ms")

    execution = result.get("agent_execution") or {}
    trial["minutes"] = _minutes(execution.get("started_at"), execution.get("finished_at"))

    # What went wrong, if anything. Harbor records both a quota wall and a
    # Docker build failure here, and it grades a quota failure like any other
    # trial — so without this field the 30 Claude trials the gateway killed
    # read as 30 losses against an arm that was never measured.
    trial["exception"] = (result.get("exception_info") or {}).get("exception_type")

    trial["passed"], trial["tests"] = _ctrf(directory)
    reasoning, answer = _text_volume(directory)
    trial["reasoning_chars"] = reasoning
    trial["answer_chars"] = answer
    return trial


def _reached_the_model(trial: dict) -> bool:
    """Whether a token was ever sent to or returned by a model.

    Zero input *and* zero output is the only reliable evidence that the agent
    never ran: a trial whose setup failed before the first request and one
    whose request was rejected by a quota wall both look like a graded 0.0, and
    a trial that worked for an hour before the quota ran out has millions.
    Reading the token count is what separates "the agent was never measured"
    from "the agent was measured and lost".
    """
    return bool(trial.get("input_tokens")) or bool(trial.get("output_tokens"))


def _has_verdict(trial: dict) -> bool:
    """Whether the verifier produced a reward at all.

    A trial can reach the model and still end without a verdict — Harbor
    records no exception for an agent phase that simply never produced a
    result frame, so `exception_info` is null and `reward_valid` is true while
    `rewards` is empty. Such a trial is not a 0.0: nothing graded it. Comparing
    two of them as a "tie" is the same mistake as comparing a quota wall to a
    real run, so the reward itself, not the exception, is what decides.
    """
    return trial.get("reward") is not None


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


def _arm_of(job_directory: str) -> str:
    """Which arm a job belongs to, from the job's own record.

    Read rather than inferred from the directory name or the order the
    directories were passed: the sweep's directories are timestamped, and a
    timestamp says nothing about which agent ran inside. Harbor records the
    agent once, in `config.json`, so that is the one place the answer cannot
    drift from.

    Anything unrecognized comes back as its own label instead of being folded
    into an arm, so a third agent in the directory is visible rather than
    silently compared as if it were the reference.
    """
    path = os.path.join(job_directory, "config.json")
    try:
        with open(path, encoding="utf-8") as handle:
            agents = json.load(handle).get("agents") or []
    except (OSError, json.JSONDecodeError):
        return "unknown"
    if not agents:
        return "unknown"
    name = (agents[0].get("name") or "").strip()
    if name == "claude-code":
        return "claude"
    if "codape" in name.lower():
        return "codape"
    return name or "unknown"


def _started_at(job_directory: str) -> str:
    """When the job started, for choosing between duplicate trials."""
    path = os.path.join(job_directory, "result.json")
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle).get("started_at") or ""
    except (OSError, json.JSONDecodeError):
        return ""


def load_arms(job_directories: list[str]) -> dict[str, dict[str, dict]]:
    """Merge every job directory into one task map per arm.

    A task can appear in more than one job of the same arm — the nine control
    tasks were run first and then again inside the sweep — so duplicates are
    resolved rather than appended. A trial that produced a `result.json` wins
    over one that is still in flight, and among two that both finished the
    later start wins: a rerun supersedes the run it repeats.

    That first rule cuts both ways, and the cutting side is worth knowing: if a
    newer job is still working a task that an older job already finished, the
    older result is what shows, because it is the only measurement there is.
    So a task can silently display a number from a different configuration than
    its neighbours. When the two jobs differ in anything that could matter —
    an environment override, an agent version — pass only the newer one and
    let the unfinished tasks read as `running`.
    """
    arms: dict[str, dict[str, dict]] = {}
    # Oldest first, so a later directory overwrites an earlier one and the
    # "last write wins" rule below is the same rule the sweep itself follows.
    for job_directory in sorted(job_directories, key=_started_at):
        arm = _arm_of(job_directory)
        trials = arms.setdefault(arm, {})
        for directory in sorted(glob.glob(os.path.join(job_directory, "*/"))):
            trial = load_trial(directory)
            previous = trials.get(trial["name"])
            if previous is None or _prefer(trial, previous):
                trials[trial["name"]] = trial
    return arms


def _prefer(candidate: dict, existing: dict) -> bool:
    """Whether `candidate` should replace `existing` for the same task."""
    if existing.get("ran") and not candidate.get("ran"):
        return False
    if candidate.get("ran") and not existing.get("ran"):
        return True
    # Both ran, or neither did. The caller iterates oldest job first, so a
    # later candidate is a rerun.
    return True


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

    wins = {"codape": 0, "claude": 0, "tie": 0, "incomplete": 0, "unmeasured": 0}
    score_rows = []
    unmeasured = []
    for name in names:
        left = codape.get(name) or {"ran": False}
        right = claude.get(name) or {"ran": False}

        def cell(trial: dict) -> str:
            if trial.get("ran"):
                # A marker rather than a column: it only matters on the runs
                # that have it, and a blank column on 95 % of rows would push
                # the numbers that do matter off the edge of the terminal.
                reran = (trial.get("result_frames") or 1) > 1
                suffix = f" x{trial['result_frames']}" if reran else ""
                # A graded trial that never reached the model says so instead
                # of printing the 0.0 Harbor gave it: that reward is the
                # harness scoring an absence, and printed as a score it is
                # indistinguishable from losing.
                if not _reached_the_model(trial):
                    return "no model" + suffix
                # Reached the model and still no reward: the agent phase ended
                # without a verdict. Its token count is real, so the run is
                # informative, but there is no score to compare.
                if not _has_verdict(trial):
                    return f"{_minutes_text(trial.get('minutes'))} no verdict{suffix}"
                return (
                    f"{_reward(trial.get('reward'))} "
                    f"{_counts(trial.get('passed'), trial.get('tests'))} "
                    f"{_minutes_text(trial.get('minutes'))}"
                    + suffix
                )
            if trial.get("present"):
                return "running"
            return "not run"

        # The tally must add to the number of rows printed, or an arm's count
        # silently depends on which tasks happened to be measured. `unmeasured`
        # is the bucket for "both arms have a directory and at least one
        # produced nothing usable", and it is counted, not skipped.
        measured_left = left.get("ran") and _reached_the_model(left)
        measured_right = right.get("ran") and _reached_the_model(right)
        delta = "-"
        if measured_left and measured_right:
            if not (_has_verdict(left) and _has_verdict(right)):
                wins["unmeasured"] += 1
                delta = "unmeasured"
                for trial in (left, right):
                    if not _has_verdict(trial):
                        unmeasured.append((name, trial))
            else:
                lp = left.get("passed") or 0
                rp = right.get("passed") or 0
                lr = left.get("reward") or 0.0
                rr = right.get("reward") or 0.0
                if lr != rr:
                    wins["codape" if lr > rr else "claude"] += 1
                    delta = "codape" if lr > rr else "claude"
                elif lp != rp:
                    # Same all-or-nothing reward; the verifier's own count is
                    # the only thing that separates them.
                    wins["codape" if lp > rp else "claude"] += 1
                    delta = f"{lp - rp:+d} tests"
                else:
                    wins["tie"] += 1
                    delta = "tie"
                score_rows.append((name, left, right))
        elif left.get("ran") and right.get("ran"):
            # Both finished, at least one never reached the model. Named
            # separately from `one-arm-only`, because "the harness never ran
            # this" and "only one arm was asked" are different gaps and only
            # one of them is fixed by running the task again.
            wins["unmeasured"] += 1
            delta = "unmeasured"
            for trial in (left, right):
                if not _reached_the_model(trial):
                    unmeasured.append((name, trial))
        else:
            wins["incomplete"] += 1

        print(f"{name:<32} {cell(left):>20} {cell(right):>20}  {delta:>7}")

    print()
    total = sum(wins.values())
    print(
        f"codape {wins['codape']}  claude {wins['claude']}  "
        f"tie {wins['tie']}  unmeasured {wins['unmeasured']}  "
        f"one-arm-only {wins['incomplete']}  ({total} tasks)"
    )
    if unmeasured:
        _unmeasured_table(unmeasured)

    _totals("codape", codape)
    _totals("claude", claude)
    _reasoning_table(score_rows)


def _unmeasured_table(rows: list[tuple[str, dict]]) -> None:
    """Why each zero-token trial has no verdict, by the exception Harbor saved.

    The exception type is the whole answer here, and it is the field that
    separates the two kinds of zero: `UnknownApiError` with a quota message
    means the account was out and the trial would have worked later;
    `AgentSetupTimeoutError` and a `ValueError` from a missing capability mean
    the run was misconfigured and would fail again. Retrying the first is
    free-ish and worth it; retrying the second is the same failure twice.
    """
    print()
    print(f"{'task':<32} {'exception':<34} {'note'}")
    print("-" * 96)
    for name, trial in rows:
        if not _reached_the_model(trial):
            # Never started. The exception is the whole answer, and it is the
            # field that separates "the account was out" from "the run was
            # misconfigured": the first works on a retry, the second does not.
            exception = trial.get("exception") or "no exception recorded"
            note = (
                "would work on a retry"
                if exception in _TRANSIENT_EXCEPTIONS
                else "will fail the same way again"
            )
        else:
            # Reached the model and was not graded. The tokens are real, so
            # this is a run that happened, not an absence.
            exception = trial.get("exception") or "no verdict, no exception"
            note = f"{trial.get('input_tokens') or 0:,} input tokens spent"
        print(f"{name:<32} {exception:<34} {note}")


# Harbor's own error policy marks these retryable, and both were observed to be
# the account, not the task: `UnknownApiError` carried `usage limit reached`
# from the gateway's weekly quota, and a setup timeout was a slow `npm install`.
_TRANSIENT_EXCEPTIONS = {
    "UnknownApiError",
    "ApiError",
    "ApiRateLimitError",
    "ApiUsageLimitError",
    "NetworkConnectionError",
    "AgentSetupTimeoutError",
    "EnvironmentStartTimeoutError",
}


def _totals(label: str, trials: dict[str, dict]) -> None:
    ran = [t for t in trials.values() if t.get("ran")]
    running = [t for t in trials.values() if t.get("present") and not t.get("ran")]
    if not ran:
        print(f"\n{label}: no finished trials"
              + (f", {len(running)} running" if running else ""))
        return
    scored = [t for t in ran if t.get("reward") == 1.0]
    minutes = [t["minutes"] for t in ran if t.get("minutes") is not None]
    output = [t["output_tokens"] for t in ran if t.get("output_tokens")]
    # Two denominators, both named. `graded` is the only one a pass rate may be
    # divided by — a quota wall produces a finished trial with a 0.0 in it, and
    # dividing by those reports an arm as worse the more of its runs the
    # gateway killed. `finished` is kept beside it because the difference is
    # itself a result: 30 of 45 ungraded is the most important fact about the
    # Claude arm.
    graded = [t for t in ran if _reached_the_model(t) and _has_verdict(t)]
    rate = (
        f" ({len(scored) / len(graded) * 100:.0f}%)" if graded else " (no rate)"
    )
    print(
        f"\n{label}: {len(scored)} solved of {len(graded)} graded"
        f"{rate}, "
        f"{len(ran)} finished{_unmeasured_breakdown(ran)}, "
        f"{sum(minutes):.0f} min total"
        + (f", {sum(output):,} output tokens" if output else "")
        # An in-flight trial has no score yet. Counting it either way would be
        # wrong: as a failure it understates, as an omission it hides how much
        # of the arm is still unmeasured.
        + (f"  [{len(running)} still running]" if running else "")
    )


def _unmeasured_breakdown(trials: list[dict]) -> str:
    """Why the finished trials are not all graded, in one parenthetical."""
    no_model = sum(1 for t in trials if not _reached_the_model(t))
    no_verdict = sum(
        1
        for t in trials
        if _reached_the_model(t) and not _has_verdict(t)
    )
    parts = []
    if no_model:
        parts.append(f"{no_model} never reached the model")
    if no_verdict:
        parts.append(f"{no_verdict} no verdict")
    return f" ({', '.join(parts)})" if parts else ""


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
    if len(sys.argv) < 2:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        print("usage: analyze_sweep.py <job-dir>...", file=sys.stderr)
        return 2
    job_directories = sys.argv[1:]
    for directory in job_directories:
        if not os.path.isdir(directory):
            print(f"not a directory: {directory}", file=sys.stderr)
            return 1
    arms = load_arms(job_directories)
    for label in ("codape", "claude"):
        arms.setdefault(label, {})
    report(arms["codape"], arms["claude"])
    extra = sorted(set(arms) - {"codape", "claude"})
    if extra:
        print()
        print(f"ignored job(s) from other agents: {', '.join(extra)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
