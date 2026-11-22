#!/usr/bin/env python3
"""Check the runners' transcript parsing without Docker, a model, or the harness.

Every number the three external runners print comes from one of these
functions, and each of them had a version that read only the *last* `result`
frame. Codape writes one frame per continuation segment, so that version
understated a compacted run — and nothing caught it, because the only way to run
the runners was a paid benchmark. This is the cheap half: synthetic transcripts
and a temporary directory.

It imports the runner modules directly. `codape_agent.py` imports
``terminal_bench``, which is not installed here and must not be (it is pinned to
Python 3.12 and pulled into a venv only for a real run), so the three names the
module touches at import time are stubbed. Nothing else about the module is
faked; the functions under test are the real ones.

Usage:
    python3 -I scripts/external-benchmarks/check_reporting.py
"""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import types
from pathlib import Path

ROOT = Path(__file__).resolve().parent
FAILURES: list[str] = []


def check(condition: bool, message: str) -> None:
    if not condition:
        FAILURES.append(message)


def load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise SystemExit(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def stub_terminal_bench() -> None:
    """The three imports `codape_agent.py` performs, and nothing more."""
    for package in (
        "terminal_bench",
        "terminal_bench.agents",
        "terminal_bench.agents.installed_agents",
        "terminal_bench.terminal",
    ):
        module = types.ModuleType(package)
        module.__path__ = []  # a namespace for the submodules below
        sys.modules[package] = module

    base_agent = types.ModuleType("terminal_bench.agents.base_agent")

    class AgentResult:
        def __init__(self, **fields):
            self.__dict__.update(fields)

    base_agent.AgentResult = AgentResult
    sys.modules[base_agent.__name__] = base_agent

    failure_mode = types.ModuleType("terminal_bench.agents.failure_mode")

    class FailureMode:
        NONE = "none"
        AGENT_TIMEOUT = "agent_timeout"
        UNKNOWN_AGENT_ERROR = "unknown_agent_error"

    failure_mode.FailureMode = FailureMode
    sys.modules[failure_mode.__name__] = failure_mode

    installed = types.ModuleType(
        "terminal_bench.agents.installed_agents.abstract_installed_agent"
    )

    class AbstractInstalledAgent:
        def __init__(self, **kwargs):
            del kwargs

    installed.AbstractInstalledAgent = AbstractInstalledAgent
    sys.modules[installed.__name__] = installed

    models = types.ModuleType("terminal_bench.terminal.models")

    class TerminalCommand:
        def __init__(self, **fields):
            self.__dict__.update(fields)

    models.TerminalCommand = TerminalCommand
    sys.modules[models.__name__] = models


def transcript(*frames: dict) -> str:
    return "\n".join(json.dumps(frame) for frame in frames) + "\nnot json at all\n"


# Two continuation segments of one run. `duration_ms`/`num_turns`/`model_ms`/
# `tool_ms` describe their own submit, so the run's values are their sum.
# `usage` and `total_cost_usd` are session totals at the moment the frame was
# written, so the second frame's numbers already include the first's — summing
# those would count segment one twice per later segment. The two cache
# spellings differ on purpose so the fallback is exercised.
SEGMENTS = (
    {
        "type": "result",
        "subtype": "error_max_turns",
        "is_error": True,
        "duration_ms": 1000,
        "num_turns": 10,
        "model_ms": 800,
        "tool_ms": 150,
        "usage": {
            "input_tokens": 250,
            "output_tokens": 100,
            "cache_read_input_tokens": 1400,
            "cache_creation_input_tokens": 12,
        },
        "total_cost_usd": 0.10,
    },
    {
        "type": "result",
        "subtype": "success",
        "is_error": False,
        "duration_ms": 2000,
        "num_turns": 20,
        "model_ms": 1500,
        "tool_ms": 400,
        "usage": {
            "input_tokens": 300,
            "output_tokens": 120,
            "cache_read_tokens": 1600,
            "cache_creation_tokens": 15,
        },
        "total_cost_usd": 0.25,
    },
)
# What the run as a whole did: timing summed, usage and cost from the last
# frame that carried them.
EXPECTED_USAGE = {
    "input_tokens": 300,
    "output_tokens": 120,
    "cache_read_input_tokens": 1600,
    "cache_creation_input_tokens": 15,
}


def check_agent_module() -> None:
    stub_terminal_bench()
    agent = load("codape_agent_under_test", ROOT / "terminal-bench" / "codape_agent.py")

    summary = agent.parse_result_summary(transcript({"type": "system", "subtype": "init"}, *SEGMENTS))
    assert summary is not None
    check(summary["subtype"] == "success", "agent: the last frame's subtype must win")
    check(summary["is_error"] is False, "agent: the last frame's is_error must win")
    check(summary["result_frames"] == 2, f"agent: expected 2 frames, got {summary['result_frames']}")
    check(summary["num_turns"] == 30, f"agent: turns not summed: {summary['num_turns']}")
    check(summary["duration_ms"] == 3000, f"agent: duration not summed: {summary['duration_ms']}")
    check(summary["model_ms"] == 2300, f"agent: model_ms not summed: {summary['model_ms']}")
    check(summary["tool_ms"] == 550, f"agent: tool_ms not summed: {summary['tool_ms']}")
    check(
        summary["usage"] == EXPECTED_USAGE,
        "agent: usage must be the last frame's session total, not a sum of segments "
        f"(got {summary['usage']})",
    )
    check(summary["total_cost_usd"] == 0.25, "agent: cost must be the last frame's total")
    check(agent.parse_result_summary("nothing here\n") is None, "agent: no frame must mean None")

    # A binary that predates usage reporting: the fields must be absent, not 0.
    legacy = agent.parse_result_summary(
        transcript(
            {
                "type": "result",
                "subtype": "success",
                "is_error": False,
                "duration_ms": 5,
                "num_turns": 1,
            }
        )
    )
    assert legacy is not None
    check(legacy["usage"] is None, "agent: an unreported usage must be None, not a zeroed object")
    check(legacy["total_cost_usd"] is None, "agent: an unreported cost must be None")


def check_terminal_bench_runner() -> None:
    runner = load(
        "run_terminal_bench_under_test",
        ROOT / "terminal-bench" / "run_terminal_bench.py",
    )
    with tempfile.TemporaryDirectory() as tmp:
        run_dir = Path(tmp)

        def trial(task: str, attempt: int, resolved: bool, mode: str, record: dict | None):
            name = f"{task}.{attempt}-of-2.r1"
            sessions = run_dir / task / name / "sessions"
            sessions.mkdir(parents=True)
            if record is not None:
                (sessions / "codape-run.json").write_text(json.dumps(record), encoding="utf-8")
            return {
                "task_id": task,
                "trial_name": name,
                "is_resolved": resolved,
                "failure_mode": mode,
                "total_input_tokens": 10,
                "total_output_tokens": 5,
            }

        full_record = {
            "model_ms": 1000,
            "tool_ms": 100,
            "duration_ms": 1200,
            "num_turns": 4,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 3,
                "cache_creation_input_tokens": 1,
            },
            "total_cost_usd": 0.01,
        }
        trials = [
            trial("a", 1, True, "unset", full_record),
            trial(
                "a",
                2,
                False,
                "unset",
                {
                    "model_ms": 900,
                    "tool_ms": 300,
                    "duration_ms": 1300,
                    "num_turns": 5,
                    "usage": {
                        "input_tokens": 20,
                        "output_tokens": 6,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                    },
                },
            ),
            trial("b", 1, False, "test_timeout", None),
            trial("c", 1, False, "parse_error", None),
            # A trial whose binary recorded timing as None is still scored; it
            # just contributes nothing to the sums.
            trial("d", 1, True, "unset", {"model_ms": None, "tool_ms": None}),
        ]
        summary = runner.summarize_results({"results": trials}, run_dir)

        check(summary["total"] == 5, "tb: total")
        check(summary["resolved"] == 2, "tb: resolved")
        check(
            summary["plumbing_failures"] == ["parse_error", "test_timeout"],
            f"tb: plumbing failures not identified: {summary['plumbing_failures']}",
        )
        check(summary["scored"] == 3, f"tb: scored count {summary['scored']}")
        check(summary["scored_resolved"] == 2, "tb: scored resolved")
        check(
            summary["scored_accuracy"] is not None
            and abs(summary["scored_accuracy"] - 2 / 3) < 1e-9,
            f"tb: scored accuracy {summary['scored_accuracy']}",
        )
        # The plumbing failures stay in the harness definition, so the two
        # numbers differ exactly when plumbing did.
        check(abs(summary["accuracy"] - 0.4) < 1e-9, f"tb: accuracy {summary['accuracy']}")
        check(summary["model_ms"] == 1900, f"tb: model_ms {summary['model_ms']}")
        check(summary["tool_ms"] == 400, f"tb: tool_ms {summary['tool_ms']}")
        check(summary["num_turns"] == 9, f"tb: num_turns {summary['num_turns']}")
        check(summary["timing_records"] == 3, f"tb: timing_records {summary['timing_records']}")
        check(summary["input_tokens_stream"] == 30, "tb: streamed input tokens")
        check(summary["total_cost_usd"] == 0.01, "tb: cost sum")
        check(summary["priced_instances"] == 1, "tb: priced instances")

        # No sidecars at all: every timing field is None, never 0. A missing
        # measurement and a measured zero are different facts.
        bare = runner.summarize_results({"results": trials}, Path(tmp) / "absent")
        check(bare["model_ms"] is None, "tb: absent timing must be None")
        check(bare["timing_records"] == 0, "tb: absent timing count")


def check_swebench_runner() -> None:
    runner = load("run_swebench_under_test", ROOT / "swebench" / "run_swebench.py")
    summary = runner.parse_stream_json_summary(transcript(*SEGMENTS))
    check(summary["result_frames"] == 2, "swe: result frames")
    check(summary["num_turns"] == 30, f"swe: turns not summed: {summary['num_turns']}")
    check(summary["model_ms"] == 2300, f"swe: model_ms {summary['model_ms']}")
    check(summary["tool_ms"] == 550, f"swe: tool_ms {summary['tool_ms']}")
    check(summary["total_cost_usd"] == 0.25, "swe: cost")
    usage = summary["usage"]
    check(usage is not None and usage["input_tokens"] == 300, "swe: input tokens")
    check(
        usage is not None and usage["cache_read_input_tokens"] == 1600,
        "swe: cache read fallback",
    )
    check(
        usage == EXPECTED_USAGE,
        "swe: usage must be the last frame's session total, not a sum of segments "
        f"(got {usage})",
    )
    check(runner.parse_stream_json_summary("")["usage"] is None, "swe: empty stream")
    check(runner.parse_stream_json_usage("") is None, "swe: empty stream usage")
    usage_only = runner.parse_stream_json_usage(transcript(*SEGMENTS))
    check(
        usage_only is not None and usage_only["total_cost_usd"] == 0.25,
        "swe: the usage view must still carry the cost",
    )
    check_swebench_attempt_aggregation(runner)


def check_swebench_attempt_aggregation(runner) -> None:
    """pass@1 over `--n-attempts`, and that one attempt is unchanged.

    With repeats the runner grades each attempt separately, because the
    official harness keys a predictions file by `instance_id`. The score is
    then over trials, and it has to agree with the resolved/total pair printed
    beside it — a summary that says "3/3 resolved, pass@1 0.83" is worse than
    no summary. This is arithmetic no real run would check until it had
    already spent the money.
    """
    args = types.SimpleNamespace(
        dataset="verified",
        run_id="check",
        model_name="model",
        _dataset={"revision": "r", "repo": "o"},
    )
    selected = [{"instance_id": iid} for iid in ("A", "B", "C")]
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        # Attempt 1 fixes 2 of 3, attempt 2 fixes all 3: 5 of 6 trials.
        reports = [
            {"total_instances": 3, "resolved_instances": 2},
            {"total_instances": 3, "resolved_instances": 3},
        ]
        entries = []
        for index, report in enumerate(reports, 1):
            path = root / f"report-{index}.json"
            path.write_text(json.dumps(report), encoding="utf-8")
            entries.append({"attempt": index, "report": str(path), "returncode": 0})
        records = [
            {"instance_id": "A", "attempt": 1, "model_ms": 10, "tool_ms": 5, "num_turns": 2},
            {"instance_id": "A", "attempt": 2, "model_ms": 10, "tool_ms": 5, "num_turns": 2},
        ]
        summary = runner.summarize(
            args, root, records, None, {}, root / "predictions.jsonl", selected, attempt_reports=entries
        )
        check(summary["pass_at_1"] is not None, "swe: pass@1 missing with two attempt reports")
        if summary["pass_at_1"] is not None:
            check(
                abs(summary["pass_at_1"] - 5 / 6) < 1e-9,
                f"swe: pass@1 over two attempts must be 5/6, got {summary['pass_at_1']}",
            )
            check(
                summary["total_instances"] == 6 and summary["resolved_instances"] == 5,
                "swe: resolved/total must be counted over trials, so it agrees with "
                f"pass@1 (got {summary['resolved_instances']}/{summary['total_instances']})",
            )
        check(summary["attempts"] == 2, f"swe: attempts recorded as {summary['attempts']}")
        check(
            len(summary["evaluation"]["attempts"]) == 2,
            "swe: each attempt's report must be listed in summary.json",
        )

        # One attempt must produce exactly what it produced before repeats
        # existed: resolved / total straight from the report.
        single = root / "single.json"
        single.write_text(
            json.dumps({"total_instances": 24, "resolved_instances": 21}), encoding="utf-8"
        )
        one = runner.summarize(args, root, [], single, {}, root / "p.jsonl", selected)
        check(
            one["total_instances"] == 24 and one["resolved_instances"] == 21,
            "swe: a single attempt must report the report's own counts",
        )
        check(
            one["pass_at_1"] is not None and abs(one["pass_at_1"] - 0.875) < 1e-9,
            f"swe: a single attempt must still be 21/24, got {one['pass_at_1']}",
        )


def check_programbench_runner() -> None:
    runner = load(
        "run_programbench_under_test",
        ROOT / "programbench" / "run_programbench.py",
    )
    with tempfile.TemporaryDirectory() as tmp:
        stream = Path(tmp) / "codape-stream.jsonl"
        stream.write_text(transcript(*SEGMENTS), encoding="utf-8")
        message = runner.summarize_stream(stream)
        check("model_ms=2300" in message, f"pb: the log line lost the timing: {message}")
        sidecar = stream.with_suffix(".summary.json")
        check(sidecar.exists(), "pb: no summary sidecar")
        if sidecar.exists():
            record = json.loads(sidecar.read_text(encoding="utf-8"))
            check(record["model_ms"] == 2300, f"pb: sidecar model_ms {record['model_ms']}")
            check(record["tool_ms"] == 550, f"pb: sidecar tool_ms {record['tool_ms']}")
            check(record["num_turns"] == 30, f"pb: sidecar turns {record['num_turns']}")
            check(
                record["usage"] == EXPECTED_USAGE,
                f"pb: sidecar usage must be the session total: {record['usage']}",
            )
            check(record["total_cost_usd"] == 0.25, "pb: sidecar cost")


def main() -> int:
    # Each group runs inside a guard: the code this replaces had no such fields
    # at all, so a raw `KeyError` there would stop at the first one instead of
    # saying which fields the runners are missing.
    for group in (
        check_agent_module,
        check_terminal_bench_runner,
        check_swebench_runner,
        check_programbench_runner,
    ):
        try:
            group()
        except (KeyError, IndexError, TypeError, AttributeError) as exc:
            FAILURES.append(f"{group.__name__}: {exc!r} (a field the runner does not report)")
    if FAILURES:
        for failure in FAILURES:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print("benchmark reporting: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
