#!/usr/bin/env python3
"""Run Codape on a Terminal-Bench subset through the ``tb`` harness.

This wrapper exists so that the invocation is one command, every version is
pinned, and ``--dry-run`` works without the harness installed. The adapter
itself lives in ``codape_agent.py`` next to this file and is passed to ``tb``
with ``--agent-import-path codape_agent:CodapeAgent``.

Terminal-Bench builds a Docker image per task, installs the agent into the
container, lets it work in a tmux session, then runs the task's tests. It is
slow and expensive; it is not part of normal CI. See ../README.md.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
ADAPTER_DIR = Path(__file__).resolve().parent

# Pins. Changing these changes what the score means.
TERMINAL_BENCH_VERSION = "0.2.18"
TERMINAL_BENCH_PYTHON = "3.12"
DATASET = "terminal-bench-core==0.1.1"
CODAPE_VERSION = "v0.2.2"
ADAPTER_IMPORT_PATH = "codape_agent:CodapeAgent"
CLAUDE_ADAPTER_IMPORT_PATH = "claude_agent:ClaudeAgent"
CLAUDE_CODE_VERSION = "2.1.318"

DEFAULT_TIMEOUT = 1800
DEFAULT_MAX_TURNS = 300
DEFAULT_N_CONCURRENT = 1

# The grading phase has to fit the dependencies the test installs itself. The
# tasks declare 20-600 s and 56 of the 80 declare 60 s, which is less than
# `pip install torch` (283 MB) or an `apt-get install` on a slow link — so a
# correct answer is scored `test_timeout` before the test it is meant to run
# starts. Measured: `pytorch-model-cli.easy` r2 never reached pytest (`pip
# install` of torch and torchvision consumed the whole 120 s), and
# `cartpole-rl-training` declares 60 s around its own torch install. 900 s is
# above every value the dataset declares, so as a global override this only
# ever raises one. A genuinely hung test now burns 900 s instead of 60, which
# is the price of never mistaking a slow install for a failed task.
DEFAULT_TEST_TIMEOUT = 900
DEFAULT_N_ATTEMPTS = 1

# Harness outcomes that are not verdicts on the agent: the grading test never
# produced one (it timed out, its own bootstrap died, or never parsed), or the
# agent was never installed. `docs/terminal-bench.md` carries the same table.
# Everything else — including `unknown_agent_error`, which is what a Codape
# result frame reporting an error maps to — is a scored run.
PLUMBING_FAILURE_MODES = {"test_timeout", "parse_error", "agent_installation_failed"}

API_KEY_ENV = (
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CODAPE_API_KEY",
    "CODAPE_AUTH_TOKEN",
    "OPENAI_API_KEY",
)
MODEL_ENV = ("ANTHROPIC_MODEL", "CODAPE_MODEL", "OPENAI_MODEL")


class BenchError(RuntimeError):
    pass


def utc_stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def resolve_cache_dir(explicit: str | None) -> Path:
    if explicit:
        return Path(explicit).expanduser().resolve()
    env = os.environ.get("CODAPE_BENCH_CACHE")
    if env:
        return Path(env).expanduser().resolve()
    return Path.home() / ".cache" / "codape-external-benchmarks"


def venv_dir(cache_dir: Path) -> Path:
    return cache_dir / f"venv-terminal-bench-{TERMINAL_BENCH_VERSION}"


def venv_python(cache_dir: Path) -> Path:
    return venv_dir(cache_dir) / "bin" / "python"


def tb_bin(cache_dir: Path) -> Path:
    return venv_dir(cache_dir) / "bin" / "tb"


def venv_install_commands(cache_dir: Path) -> list[list[str]]:
    venv = venv_dir(cache_dir)
    if shutil.which("uv"):
        return [
            ["uv", "venv", "--python", TERMINAL_BENCH_PYTHON, str(venv)],
            [
                "uv",
                "pip",
                "install",
                "--python",
                str(venv_python(cache_dir)),
                f"terminal-bench=={TERMINAL_BENCH_VERSION}",
            ],
        ]
    return [
        [sys.executable, "-m", "venv", str(venv)],
        [str(venv / "bin" / "pip"), "install", "--upgrade", "pip"],
        [
            str(venv / "bin" / "pip"),
            "install",
            f"terminal-bench=={TERMINAL_BENCH_VERSION}",
        ],
    ]


def python_version_ok() -> tuple[bool, str]:
    if sys.version_info >= (3, 12):
        return True, f"python {sys.version.split()[0]}"
    return False, (
        f"python {sys.version.split()[0]} is too old for terminal-bench "
        f"{TERMINAL_BENCH_VERSION} (requires >= {TERMINAL_BENCH_PYTHON}); install uv "
        "or run with a Python 3.12+ interpreter"
    )


def venv_is_ready(cache_dir: Path) -> bool:
    python = venv_python(cache_dir)
    tb = tb_bin(cache_dir)
    if not python.exists() or not tb.exists():
        return False
    probe = (
        "from importlib.metadata import version\n"
        f"assert version('terminal-bench') == '{TERMINAL_BENCH_VERSION}', version('terminal-bench')\n"
        "import terminal_bench\n"
    )
    result = subprocess.run(
        [str(python), "-I", "-c", probe],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        check=False,
    )
    return result.returncode == 0


def ensure_venv(cache_dir: Path) -> None:
    if venv_is_ready(cache_dir):
        return
    if not shutil.which("uv"):
        ok, detail = python_version_ok()
        if not ok:
            raise BenchError(detail)
    cache_dir.mkdir(parents=True, exist_ok=True)
    print(f"creating Terminal-Bench venv in {venv_dir(cache_dir)}", flush=True)
    for command in venv_install_commands(cache_dir):
        print("+ " + " ".join(command), flush=True)
        if subprocess.run(command, check=False).returncode != 0:
            raise BenchError(f"failed to prepare the terminal-bench venv: {' '.join(command)}")
    if not venv_is_ready(cache_dir):
        raise BenchError(
            f"the venv was created but terminal-bench=={TERMINAL_BENCH_VERSION} is not importable"
        )


def reexec_in_venv(cache_dir: Path) -> None:
    target = venv_python(cache_dir)
    if os.environ.get("CODAPE_TB_VENV") == str(target):
        return
    if Path(sys.executable).resolve() == target.resolve():
        return
    env = os.environ.copy()
    env["CODAPE_TB_VENV"] = str(target)
    os.execve(str(target), [str(target), str(Path(__file__).resolve()), *sys.argv[1:]], env)


def looks_like_linux_elf(path: Path) -> bool:
    try:
        with path.open("rb") as handle:
            return handle.read(4) == b"\x7fELF"
    except OSError:
        return False


def docker_available() -> tuple[bool, str]:
    if shutil.which("docker") is None:
        return False, "docker CLI not found on PATH"
    try:
        result = subprocess.run(
            ["docker", "info"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return False, f"docker info failed: {exc}"
    if result.returncode != 0:
        detail = (result.stderr or "").strip().splitlines()
        return False, detail[-1] if detail else "docker daemon is not reachable"
    return True, "docker is ready"


def require_api_key() -> None:
    if not any(os.environ.get(name) for name in API_KEY_ENV):
        raise BenchError(
            "no model API key found in the environment. Set one of "
            + ", ".join(API_KEY_ENV)
            + " (and optionally ANTHROPIC_BASE_URL / ANTHROPIC_MODEL)."
        )


def model_label() -> str:
    for name in MODEL_ENV:
        value = os.environ.get(name)
        if value:
            return value
    return "default"


def build_tb_command(args: argparse.Namespace, cache_dir: Path, output_dir: Path) -> list[str]:
    command = [
        str(tb_bin(cache_dir)),
        "run",
        # `--dataset-path` and `--dataset` are mutex: the harness refuses a
        # config that has both. The offline path wins when it is given.
        *(
            ["--dataset-path", str(Path(args.dataset_path).expanduser())]
            if getattr(args, "dataset_path", None)
            else ["--dataset", args.dataset]
        ),
        "--output-path",
        str(output_dir),
        "--run-id",
        args.run_id,
        "--n-concurrent",
        str(args.n_concurrent),
        "--n-attempts",
        str(args.n_attempts),
        "--agent-import-path",
        ADAPTER_IMPORT_PATH if args.agent == "codape" else CLAUDE_ADAPTER_IMPORT_PATH,
        "--agent-kwarg",
        f"version={args.codape_version if args.agent == 'codape' else args.claude_version}",
        "--agent-kwarg",
        f"max_turns={args.max_turns}",
        "--agent-kwarg",
        f"timeout_sec={args.timeout}",
        "--global-agent-timeout-sec",
        str(args.timeout + 180),
    ]
    # The test budget has to fit the dependencies the test installs itself. The
    # tasks' own `max_test_timeout_sec` values are shorter than `pip install
    # torch` on several of them, which scores a correct answer as
    # `test_timeout` before pytest is ever reached. Raising it changes nothing
    # for a test that finishes early; `--test-timeout 0` defers to each task.
    if args.test_timeout:
        command += ["--global-test-timeout-sec", str(args.test_timeout)]
    if args.model:
        command += ["--model", args.model]
    if args.agent == "codape":
        if args.codape_binary:
            command += ["--agent-kwarg", f"binary_path={args.codape_binary}"]
        if args.extra_args:
            command += ["--agent-kwarg", f"extra_args={' '.join(args.extra_args)}"]
        if args.extra_instructions:
            command += [
                "--agent-kwarg",
                f"extra_instructions={args.extra_instructions}",
            ]
        if args.task_proxy:
            command += ["--agent-kwarg", f"task_proxy={args.task_proxy}"]
        if args.task_proxy_direct_hosts:
            command += [
                "--agent-kwarg",
                f"task_proxy_direct_hosts={args.task_proxy_direct_hosts}",
            ]
    if args.tasks:
        for task in args.tasks:
            command += ["--task-id", task]
    elif args.limit is not None:
        command += ["--n-tasks", str(args.limit)]
    return command


def dry_run(args: argparse.Namespace, cache_dir: Path, output_dir: Path) -> None:
    command = build_tb_command(args, cache_dir, output_dir)
    print("Terminal-Bench dry run; nothing will be downloaded, built, or executed.")
    print(f"pinned harness : terminal-bench=={TERMINAL_BENCH_VERSION}")
    print(f"pinned dataset : {args.dataset}")
    if args.agent == "codape":
        print(f"adapter        : {ADAPTER_IMPORT_PATH} ({ADAPTER_DIR / 'codape_agent.py'})")
        print(f"adapter install: {args.codape_version} ({ADAPTER_DIR / 'codape-setup.sh.j2'})")
    else:
        print(f"adapter        : {CLAUDE_ADAPTER_IMPORT_PATH} ({ADAPTER_DIR / 'claude_agent.py'})")
        print(f"adapter install: Claude Code {args.claude_version} (nvm + npm inside each task container)")
    if args.codape_binary:
        print(f"codape binary : {args.codape_binary}")
    print(f"attempts       : {args.n_attempts} per task, {args.n_concurrent} concurrent")
    print(f"venv cache     : {venv_dir(cache_dir)}")
    print(f"output         : {output_dir / args.run_id}")
    print(f"results        : {output_dir / args.run_id / 'results.json'}")
    print()

    ready, message = docker_available()
    if ready:
        print(f"docker         : {message}")
    else:
        print(f"docker         : NOT AVAILABLE ({message}) -- dry run continues; a real run needs Docker")
    if any(os.environ.get(name) for name in API_KEY_ENV):
        print("api key        : set in the environment (value not printed)")
    else:
        print("api key        : NOT SET (a real run needs one)")
    print(f"model          : {args.model or model_label()} (from --model or environment)")
    if not args.codape_binary and platform.machine().lower() in ("arm64", "aarch64"):
        print(
            "architecture   : this host is arm64, so task images are usually linux/arm64 "
            "and the pinned x86_64 Codape release will not run; use an x86_64 host or "
            "pass --codape-binary with an aarch64 Linux build"
        )
    print()

    print("# 1. Prepare the isolated harness venv")
    for install in venv_install_commands(cache_dir):
        print("+ " + " ".join(install))
    print()

    print("# 2. tb downloads the pinned dataset and builds one Docker image per task")
    print("#    (network required; the dataset is cached under ~/.cache/terminal-bench)")
    print()

    print(f"# 3. Run {args.agent} through the adapter")
    print("+ PYTHONPATH=" + str(ADAPTER_DIR) + " " + " ".join(command))
    print()

    print("# 4. tb writes results.json (accuracy, n_resolved, per-trial failures)")
    print("Terminal-Bench dry run complete.")


def parse_results(path: Path) -> dict:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise BenchError(f"could not read results.json at {path}: {exc}") from exc
    if "results" not in data or not isinstance(data["results"], list):
        raise BenchError(f"unexpected results.json shape at {path}")
    return data


def read_trial_record(run_dir: Path, trial: dict) -> dict | None:
    """Read the adapter's per-trial telemetry, if the adapter wrote one.

    The harness's `results.json` carries tokens but not where a turn's time
    went, and it cannot be extended from here. The adapter writes
    `/logs/codape-run.json` inside the container, which the harness mounts to
    `<task>/<trial>/sessions/`, so the file is on the host after the trial.
    """
    task_id = trial.get("task_id")
    trial_name = trial.get("trial_name")
    if not task_id or not trial_name:
        return None
    path = run_dir / str(task_id) / str(trial_name) / "sessions" / "codape-run.json"
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def summarize_results(data: dict, run_dir: Path | None = None) -> dict:
    trials = data.get("results", [])
    resolved = [trial for trial in trials if trial.get("is_resolved")]
    total = len(trials)
    input_tokens = sum(trial.get("total_input_tokens") or 0 for trial in trials)
    output_tokens = sum(trial.get("total_output_tokens") or 0 for trial in trials)
    failure_modes = {
        str(trial.get("failure_mode"))
        for trial in trials
        if trial.get("failure_mode") not in (None, "none", "unset", "None")
    }
    # A plumbing failure is the harness or the task's own bootstrap failing, so
    # the trial never produced a verdict on the agent. Counting those in the
    # denominator makes a score depend on the network. `pass_at_1` stays the
    # harness definition (resolved / attempted) so it is comparable with other
    # published runs; `scored` and `scored_accuracy` are the honest pair.
    scored_trials = [
        trial for trial in trials if trial.get("failure_mode") not in PLUMBING_FAILURE_MODES
    ]
    plumbing = [
        str(trial.get("failure_mode"))
        for trial in trials
        if trial.get("failure_mode") in PLUMBING_FAILURE_MODES
    ]
    summary = {
        "total": total,
        "resolved": len(resolved),
        "accuracy": len(resolved) / total if total else 0.0,
        "scored": len(scored_trials),
        "scored_resolved": sum(1 for trial in scored_trials if trial.get("is_resolved")),
        "scored_accuracy": (
            sum(1 for trial in scored_trials if trial.get("is_resolved")) / len(scored_trials)
            if scored_trials
            else None
        ),
        "plumbing_failures": sorted(plumbing),
        "resolved_ids": [trial.get("task_id") for trial in resolved],
        "failure_modes": sorted(failure_modes),
        "input_tokens": input_tokens or None,
        "output_tokens": output_tokens or None,
    }
    if run_dir is None:
        return summary
    records = [
        record
        for trial in trials
        if (record := read_trial_record(run_dir, trial)) is not None
    ]
    summary["timing_records"] = len(records)

    def total_of(key: str) -> int | None:
        values = [record.get(key) for record in records if record.get(key) is not None]
        return sum(int(value) for value in values) if values else None

    def usage_of(key: str) -> int | None:
        values = [
            (record.get("usage") or {}).get(key)
            for record in records
            if (record.get("usage") or {}).get(key) is not None
        ]
        return sum(int(value) for value in values) if values else None

    summary["model_ms"] = total_of("model_ms")
    summary["tool_ms"] = total_of("tool_ms")
    summary["duration_ms"] = total_of("duration_ms")
    summary["num_turns"] = total_of("num_turns")
    summary["input_tokens_stream"] = usage_of("input_tokens")
    summary["output_tokens_stream"] = usage_of("output_tokens")
    summary["cache_read_input_tokens"] = usage_of("cache_read_input_tokens")
    summary["cache_creation_input_tokens"] = usage_of("cache_creation_input_tokens")
    costs = [record.get("total_cost_usd") for record in records if record.get("total_cost_usd") is not None]
    summary["total_cost_usd"] = sum(costs) if costs else None
    summary["priced_instances"] = len(costs)
    return summary


def positive_int(value: str) -> int:
    try:
        parsed = int(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("must be an integer") from exc
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return parsed


def positive_float(value: str) -> float:
    try:
        parsed = float(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("must be a number") from exc
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be > 0")
    return parsed


def floor_float(value: str) -> float:
    """A timeout where 0 means "leave the task's own value alone"."""
    try:
        parsed = float(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("must be a number") from exc
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return parsed


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run Codape on a pinned Terminal-Bench dataset through the official tb harness.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  # Print the full invocation without Docker, keys, or downloads
  scripts/external-benchmarks/terminal-bench/run.sh --dry-run --limit 1

  # One task end to end (Docker must be running)
  export ANTHROPIC_API_KEY=... ANTHROPIC_BASE_URL=... ANTHROPIC_MODEL=...
  scripts/external-benchmarks/terminal-bench/run.sh \\
      --task hello-world --timeout 900

  # A locally built Linux binary instead of the pinned release
  scripts/external-benchmarks/terminal-bench/run.sh \\
      --codape-binary target/x86_64-unknown-linux-musl/release/codape --limit 1

Notes:
  * --limit maps to tb's --n-tasks. For an exactly reproducible subset, name
    tasks with --task; that is also the only form stable across dataset
    versions, which is why the dataset is pinned here as well.
  * The Codape release is downloaded inside each task container from GitHub;
    the container needs outbound network access.
  * API keys are read from the environment. They are written only to a
    container-local setup file and are never stored in this repository.
""",
    )
    parser.add_argument(
        "--dataset",
        default=DATASET,
        help=f"Pinned dataset, name==version (default: {DATASET})",
    )
    parser.add_argument(
        "--dataset-path",
        default=None,
        help="Read task definitions from this local directory instead of "
        "asking the Terminal-Bench registry. The registry is a GitHub raw "
        "URL; when it is unreachable the whole run dies before a single task "
        "starts. Point this at an unpacked copy of the pinned dataset to run "
        "offline.",
    )
    parser.add_argument("-t", "--task", dest="tasks", action="append", metavar="ID", help="Task ID or glob; repeatable")
    parser.add_argument("--limit", type=positive_int, help="Run the first N tasks from the dataset")
    parser.add_argument("--model", help="Model ID passed to both tb metadata and the agent")
    parser.add_argument(
        "--agent",
        choices=("codape", "claude-code"),
        default="codape",
        help="Agent harness installed into the task container (default: codape)",
    )
    parser.add_argument(
        "--claude-version",
        default=CLAUDE_CODE_VERSION,
        help=f"Claude Code version installed with --agent claude-code (default: {CLAUDE_CODE_VERSION})",
    )
    parser.add_argument("--timeout", type=positive_float, default=DEFAULT_TIMEOUT, help=f"Per-task Codape timeout in seconds (default: {DEFAULT_TIMEOUT})")
    parser.add_argument("--max-turns", type=positive_int, default=DEFAULT_MAX_TURNS, help=f"Codape --max-turns (default: {DEFAULT_MAX_TURNS})")
    parser.add_argument(
        "--test-timeout",
        type=floor_float,
        default=DEFAULT_TEST_TIMEOUT,
        help="Grading timeout in seconds, overriding each task's "
        "max_test_timeout_sec. The declared budgets are shorter than the "
        "dependencies the tests install themselves, so the default is a floor "
        "that only ever raises one; pass 0 to use each task's own value "
        f"(default: {DEFAULT_TEST_TIMEOUT})",
    )
    parser.add_argument("--n-concurrent", type=positive_int, default=DEFAULT_N_CONCURRENT, help=f"Concurrent tasks (default: {DEFAULT_N_CONCURRENT}; each trial holds a container, so raise it only as far as RAM allows)")
    parser.add_argument(
        "--n-attempts",
        type=positive_int,
        default=DEFAULT_N_ATTEMPTS,
        help="Attempts per task. Repeated attempts are what separates a stable "
        "verdict from a lucky one, and the harness averages them into one "
        f"accuracy (default: {DEFAULT_N_ATTEMPTS})",
    )
    parser.add_argument("--codape-version", default=CODAPE_VERSION, help=f"Codape release installed in containers (default: {CODAPE_VERSION})")
    parser.add_argument("--codape-binary", type=Path, help="Linux x86_64 ELF Codape binary to copy into containers")
    parser.add_argument(
        "--extra-arg",
        dest="extra_args",
        action="append",
        default=[],
        metavar="ARG",
        help="Extra argument for Codape; repeatable, must not contain '='. "
        "Use --extra-arg=--flag for values starting with '-'.",
    )
    parser.add_argument(
        "--task-proxy",
        default=None,
        metavar="URL",
        help="Proxy URL exported inside every task container for the task's "
        "own traffic (package installs, dataset downloads). The provider "
        "endpoint always bypasses it. Use this when the container cannot "
        "reach a host the machine can, e.g. http://host.docker.internal:1087.",
    )
    parser.add_argument(
        "--task-proxy-direct-hosts",
        default=None,
        metavar="HOST[,HOST...]",
        help="Comma-separated hosts (or domains) that should bypass "
        "--task-proxy. Use it when only some destinations are unreachable: "
        "on this machine apt from ports.ubuntu.com is reachable directly and "
        "takes 66 s for a toolchain that takes over 20 minutes through the "
        "proxy, so 'ports.ubuntu.com,deb.debian.org,pypi.org' keeps package "
        "installs fast while huggingface.co still goes through.",
    )
    parser.add_argument("--run-id", help="Run identifier (default: tb-YYYYmmdd-HHMMSS)")
    parser.add_argument(
        "--extra-instructions",
        default=None,
        help="Text appended to every task instruction; use it to state the "
        "headless/target-container frame the task assumes.",
    )
    parser.add_argument("--output-dir", type=Path, default=REPO_ROOT / "target" / "external-benchmarks" / "terminal-bench", help="Root for harness output (default: target/external-benchmarks/terminal-bench)")
    parser.add_argument("--cache-dir", type=Path, help="Cache root for the harness venv (default: $CODAPE_BENCH_CACHE or ~/.cache/codape-external-benchmarks)")
    parser.add_argument("--dry-run", action="store_true", help="Print commands without installing, downloading, or running anything")
    args = parser.parse_args(argv)

    if args.tasks and args.limit is not None:
        parser.error("use --task or --limit, not both")
    if args.codape_binary:
        args.codape_binary = args.codape_binary.expanduser().resolve()
        if not args.codape_binary.exists():
            parser.error(f"--codape-binary does not exist: {args.codape_binary}")
        if not looks_like_linux_elf(args.codape_binary):
            parser.error(
                f"--codape-binary must be a Linux ELF binary: {args.codape_binary}. "
                "A macOS build cannot run in the Linux task containers; build with "
                "--target x86_64-unknown-linux-musl."
            )
    for arg in args.extra_args:
        if "=" in arg:
            parser.error(
                "--extra-arg must not contain '=' because tb splits --agent-kwarg on it"
            )
    if args.agent == "claude-code":
        if args.codape_binary:
            parser.error("--codape-binary is only used with --agent codape")
        if args.extra_args:
            parser.error("--extra-arg is only used with --agent codape")
        if args.task_proxy or args.task_proxy_direct_hosts:
            parser.error("--task-proxy options are only used with --agent codape")
        if not args.model:
            args.model = model_label()
        if args.model == "default":
            parser.error("--agent claude-code needs --model or ANTHROPIC_MODEL / CODAPE_MODEL")
    if args.run_id is None:
        args.run_id = f"tb-{utc_stamp()}"
    if not re.fullmatch(r"[A-Za-z0-9._-]+", args.run_id):
        parser.error("--run-id may contain only letters, digits, dot, underscore, and hyphen")
    args.output_dir = args.output_dir.expanduser().resolve()
    args.cache_dir = resolve_cache_dir(str(args.cache_dir) if args.cache_dir else None)
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    cache_dir = args.cache_dir
    output_dir = args.output_dir

    if args.dry_run:
        dry_run(args, cache_dir, output_dir)
        return 0

    try:
        ready, message = docker_available()
        if not ready:
            raise BenchError(f"Docker is required for a real run: {message}")
        require_api_key()
        ensure_venv(cache_dir)
        reexec_in_venv(cache_dir)

        command = build_tb_command(args, cache_dir, output_dir)
        env = os.environ.copy()
        # tb imports the adapter by module path; its console script does not
        # put the adapter's directory on sys.path.
        env["PYTHONPATH"] = str(ADAPTER_DIR) + (
            os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else ""
        )
        print("+ PYTHONPATH=" + str(ADAPTER_DIR) + " " + " ".join(command), flush=True)
        started = time.monotonic()
        result = subprocess.run(command, env=env, check=False)
        elapsed = time.monotonic() - started

        results_path = output_dir / args.run_id / "results.json"
        summary = None
        if results_path.exists():
            summary = summarize_results(parse_results(results_path), results_path.parent)
        print("\n=== Terminal-Bench summary ===")
        print(f"dataset  : {args.dataset}")
        print(f"run id   : {args.run_id}")
        print(f"model    : {args.model or model_label()}")
        print(f"agent    : {args.agent} {args.codape_version if args.agent == 'codape' else args.claude_version}")
        if summary:
            print(
                f"accuracy : {summary['accuracy']:.4f} "
                f"({summary['resolved']}/{summary['total']} resolved, pass@1 over every attempt)"
            )
            if summary["scored_accuracy"] is not None:
                print(
                    f"scored   : {summary['scored_accuracy']:.4f} "
                    f"({summary['scored_resolved']}/{summary['scored']} scored trials; "
                    "plumbing failures excluded)"
                )
            if summary["resolved_ids"]:
                print(f"resolved : {', '.join(summary['resolved_ids'])}")
            if summary["failure_modes"]:
                print(f"failures : {', '.join(summary['failure_modes'])}")
            if summary["plumbing_failures"]:
                print(f"plumbing : {', '.join(summary['plumbing_failures'])} (not scored)")
            stream_input = summary.get("input_tokens_stream")
            stream_output = summary.get("output_tokens_stream")
            if stream_input is None and stream_output is None:
                if summary["input_tokens"] is None and summary["output_tokens"] is None:
                    print("tokens   : unavailable (no usage in the run's result frames)")
                else:
                    print(
                        f"tokens   : input={summary['input_tokens'] or 0} "
                        f"output={summary['output_tokens'] or 0}"
                    )
            else:
                print(
                    f"tokens   : input={stream_input or 0} "
                    f"output={stream_output or 0} "
                    f"cache_read={summary.get('cache_read_input_tokens') or 0} "
                    f"cache_write={summary.get('cache_creation_input_tokens') or 0}"
                )
            if summary.get("model_ms") is not None:
                # Wall clock, model time, tool time: the three numbers that
                # separate "the model is slow" from "the tools are slow".
                print(
                    f"time     : model={summary['model_ms'] / 1000:.1f}s "
                    f"tools={summary['tool_ms'] / 1000:.1f}s "
                    f"turns={summary['num_turns'] or 0} "
                    f"({summary.get('timing_records') or 0} of {summary['total']} trials recorded)"
                )
            if summary.get("total_cost_usd") is not None:
                print(
                    f"cost     : ${summary['total_cost_usd']:.4f} "
                    f"({summary.get('priced_instances') or 0} of {summary['total']} trials priced)"
                )
        else:
            print(f"results  : not written ({results_path})")
        print(f"total    : {elapsed:.1f}s")
        print(f"results  : {results_path}")
        accuracy = f"{summary['accuracy']:.4f}" if summary else "na"
        resolved = summary["resolved"] if summary else "na"
        total = summary["total"] if summary else "na"
        scored = f"{summary['scored']}" if summary else "na"
        scored_accuracy = (
            f"{summary['scored_accuracy']:.4f}"
            if summary and summary["scored_accuracy"] is not None
            else "na"
        )
        tokens = (
            "present"
            if summary
            and (
                summary["input_tokens"]
                or summary["output_tokens"]
                or summary.get("input_tokens_stream")
            )
            else "na"
        )
        print(
            f"TERMINAL_BENCH_SUMMARY run_id={args.run_id} accuracy={accuracy} "
            f"resolved={resolved} total={total} scored={scored} "
            f"scored_accuracy={scored_accuracy} elapsed_sec={elapsed:.1f} tokens={tokens}"
        )
        return result.returncode
    except BenchError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
