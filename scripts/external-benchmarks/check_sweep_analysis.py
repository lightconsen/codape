#!/usr/bin/env python3
"""Check the sweep comparison's verdicts without a sweep, Docker, or a model.

`analyze_sweep.py` decides, task by task, whether one arm beat the other. It
got that wrong on the 2026-11-25 sweep in a way nothing caught: it compared
every finished trial, and Harbor grades a trial that never reached the model
like any other. The Claude arm's weekly quota ran out, 30 of its 45 trials
spent zero tokens, and the tool reported **`codape 26, claude 3, tie 16`** on a
comparison whose honest answer was `codape 5, claude 3, tie 6` with 31 tasks
unmeasured. The document built on those runs says the only defensible reading
is a tie, and the tool said the opposite.

The reason it survived is that the number was produced by a script whose only
exercise was a paid sweep — the same reason the runners read the last result
frame for months. This is the cheap half: synthetic job directories with the
fields harbor actually writes, and the verdicts asserted directly.

Usage:
    python3 -I scripts/external-benchmarks/check_sweep_analysis.py
"""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
ANALYZER = ROOT / "terminal-bench" / "analyze_sweep.py"
FAILURES: list[str] = []


def check(condition: bool, message: str) -> None:
    if not condition:
        FAILURES.append(message)


def load_analyzer():
    spec = importlib.util.spec_from_file_location("analyze_sweep", ANALYZER)
    if spec is None or spec.loader is None:
        raise SystemExit(f"cannot load {ANALYZER}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["analyze_sweep"] = module
    spec.loader.exec_module(module)
    return module


def trial(
    root: Path,
    task: str,
    *,
    reward: float | None = 1.0,
    input_tokens: int = 1_000_000,
    output_tokens: int = 10_000,
    passed: int | None = 1,
    tests: int | None = 1,
    exception: str | None = None,
) -> Path:
    """One trial directory, carrying only what the analyzer reads.

    The field names and shapes are harbor's, taken from a real sweep archive:
    `verifier_result.rewards.reward`, `agent_result.n_*_tokens`,
    `exception_info.exception_type`, `agent_execution.{started_at,finished_at}`
    and `verifier/ctrf.json`. A reward of `None` means the verifier produced no
    verdict and Harbor left `rewards` empty.
    """
    directory = root / f"{task}__probe"
    (directory / "verifier").mkdir(parents=True, exist_ok=True)
    result = {
        "reward_valid": reward is not None,
        "verifier_result": {} if reward is None else {"rewards": {"reward": reward}},
        "agent_result": {
            "n_input_tokens": input_tokens,
            "n_cache_tokens": 0,
            "n_output_tokens": output_tokens,
            "cost_usd": None,
            "metadata": {"num_turns": 10 if input_tokens else 0},
        },
        "agent_execution": {
            "started_at": "2026-11-25T00:00:00Z",
            "finished_at": "2026-11-25T00:10:00Z",
        },
        "exception_info": (
            {"exception_type": exception, "exception_message": "", "exception_traceback": ""}
            if exception
            else None
        ),
    }
    (directory / "result.json").write_text(json.dumps(result), encoding="utf-8")
    if passed is not None and tests is not None:
        (directory / "verifier" / "ctrf.json").write_text(
            json.dumps(
                {"results": {"summary": {"passed": passed, "tests": tests}, "tests": []}}
            ),
            encoding="utf-8",
        )
    return directory


def job(root: Path, name: str, agent: str, trials: list[dict]) -> Path:
    directory = root / name
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "config.json").write_text(
        json.dumps({"agents": [{"name": agent, "model_name": None}]}), encoding="utf-8"
    )
    for fields in trials:
        trial(directory, **fields)
    return directory


def check_quota_wall_is_not_a_loss(analyzer, root: Path) -> None:
    """The defect that produced `codape 26, claude 3`.

    One arm ran five tasks; the other produced a real run on two of them and a
    quota wall on the other three. The right answer is 2 compared (a win and a
    tie) and 3 unmeasured — not 3 more wins for the arm that happened to be
    billed to an account with quota left.
    """
    codape = job(root, "codape-job", "codape_harbor_agent:CodapeHarborAgent", [
        {"task": "won-by-codape", "reward": 1.0},
        {"task": "tied", "reward": 0.0, "passed": 3, "tests": 5},
        {"task": "quota-1", "reward": 0.0},
        {"task": "quota-2", "reward": 0.0},
        {"task": "quota-3", "reward": 0.0},
    ])
    claude = job(root, "claude-job", "claude-code", [
        {"task": "won-by-codape", "reward": 0.0, "passed": 0, "tests": 1},
        {"task": "tied", "reward": 0.0, "passed": 3, "tests": 5},
        {"task": "quota-1", "reward": 0.0, "input_tokens": 0, "output_tokens": 0,
         "passed": 1, "tests": 4, "exception": "UnknownApiError"},
        {"task": "quota-2", "reward": 0.0, "input_tokens": 0, "output_tokens": 0,
         "passed": 2, "tests": 6, "exception": "UnknownApiError"},
        {"task": "quota-3", "reward": 0.0, "input_tokens": 0, "output_tokens": 0,
         "passed": 0, "tests": 3, "exception": "AgentSetupTimeoutError"},
    ])

    arms = analyzer.load_arms([str(codape), str(claude)])
    text = capture_report(analyzer, arms)
    check(
        "unmeasured 3" in text,
        f"three quota walls must be reported as unmeasured, got: {text[-400:]}",
    )
    check(
        "codape 1" in text and "claude 0" in text and "tie 1" in text,
        f"expected codape 1, claude 0, tie 1 — one win and one tie: {text[-400:]}",
    )
    check(
        "no model" in text,
        "a zero-token trial must print as `no model`, not as its 0.0 reward",
    )
    check(
        "would work on a retry" in text,
        "a quota wall must be marked retryable and a setup timeout with it",
    )


def check_no_verdict_is_not_a_tie(analyzer, root: Path) -> None:
    """A trial that reached the model and was never graded is not a 0.0.

    `batched-eval-parity` ended with no `rewards` and no exception on both
    arms, and comparing the two as a "tie" claims both arms scored the same on
    a task neither was graded on. It is unmeasured, and its 12.6 M input tokens
    must still be reported as spent.
    """
    codape = job(root, "codape-job", "codape_harbor_agent:CodapeHarborAgent", [
        {"task": "unjudged", "reward": None, "input_tokens": 12_574_943},
    ])
    claude = job(root, "claude-job", "claude-code", [
        {"task": "unjudged", "reward": None, "input_tokens": 8_400_190},
    ])
    arms = analyzer.load_arms([str(codape), str(claude)])
    text = capture_report(analyzer, arms)
    check("unmeasured 1" in text, f"a no-verdict pair must be unmeasured: {text[-400:]}")
    check("tie 0" in text, f"a no-verdict pair must not be a tie: {text[-400:]}")
    check(
        "12,574,943 input tokens spent" in text,
        f"the spent tokens must still be reported: {text[-600:]}",
    )


def capture_report(analyzer, arms) -> str:
    import builtins
    import io
    from contextlib import redirect_stdout

    lines: list[str] = []

    def capture(*args, **kwargs):
        lines.append(" ".join(str(a) for a in args))

    original = builtins.print
    buffer = io.StringIO()
    try:
        builtins.print = capture
        with redirect_stdout(buffer):
            analyzer.report(arms["codape"], arms["claude"])
    finally:
        builtins.print = original
    return "\n".join(lines)


def check_totals_divide_by_graded(analyzer, root: Path) -> None:
    """The pass rate must not count trials the verifier never graded.

    Two solved and two quota walls is `2 solved of 2 graded (100%)`, not
    `2 of 4 (50%)`: the second number makes the arm look worse the more of its
    runs the gateway killed, which is the exact direction the defect pushed.
    """
    codape = job(root, "codape-job", "codape_harbor_agent:CodapeHarborAgent", [
        {"task": "solved-1", "reward": 1.0},
        {"task": "solved-2", "reward": 1.0},
        {"task": "quota-1", "reward": 0.0, "input_tokens": 0, "output_tokens": 0,
         "passed": 0, "tests": 2, "exception": "UnknownApiError"},
        {"task": "quota-2", "reward": 0.0, "input_tokens": 0, "output_tokens": 0,
         "passed": 1, "tests": 2, "exception": "ApiUsageLimitError"},
    ])
    arms = analyzer.load_arms([str(codape)])
    text = capture_report(analyzer, {"codape": arms["codape"], "claude": {}})
    check(
        "2 solved of 2 graded (100%)" in text,
        f"the pass rate must divide by graded trials only: {text[-500:]}",
    )
    check(
        "2 never reached the model" in text,
        f"the dropped trials must be named, not silently omitted: {text[-500:]}",
    )


def main() -> int:
    analyzer = load_analyzer()
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        for group in (
            check_quota_wall_is_not_a_loss,
            check_no_verdict_is_not_a_tie,
            check_totals_divide_by_graded,
        ):
            directory = root / group.__name__
            directory.mkdir()
            try:
                group(analyzer, directory)
            except Exception as exc:  # noqa: BLE001 - report, do not mask
                FAILURES.append(f"{group.__name__} raised {exc!r}")
    if FAILURES:
        print(f"{len(FAILURES)} failure(s):")
        for failure in FAILURES:
            print(f"  - {failure}")
        return 1
    print("sweep analysis: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
