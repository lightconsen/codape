#!/usr/bin/env python3
"""Check the Harbor runner's argv without Harbor, Docker, or a model.

`terminal-bench/run-harbor.sh` replaced a script that lived on a benchmark host
and nowhere else. That script's local-binary switch was spelled
`CODPE_LOCAL_BINARY` — one letter short — so its branch could not fire however it
was called, and no review ever saw the file to notice. A script in the repository
can still rot; this is what keeps it from rotting silently.

Each assertion below is a way the script can be wrong that costs money to
discover: a model the gateway rejects mid-trial, a `--version` that stamps a
release onto a build that did not run, a credential in the argv, a task list
that silently contributes nothing. The script is driven with `--dry-run` and a
stand-in for `harbor` that echoes its argv, so nothing is downloaded or billed.

Usage:
    python3 -I scripts/external-benchmarks/check_harbor_runner.py
"""

from __future__ import annotations

import json
import os
import shlex
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
RUNNER = ROOT / "terminal-bench" / "run-harbor.sh"
TASK_LISTS = ROOT / "terminal-bench"

FAILURES: list[str] = []

# A value that must never appear in a printed command or a child's argv.
SENTINEL = "sentinel-token-value-do-not-print"


def check(condition: bool, message: str) -> None:
    if not condition:
        FAILURES.append(message)


def write_key_file(directory: Path, model: str = "deepseek-v4.1-flash") -> Path:
    path = directory / "env.json"
    path.write_text(
        json.dumps(
            {
                "CODAPE_AUTH_TOKEN": SENTINEL,
                "CODAPE_BASE_URL": "https://gateway.invalid/v1",
                "CODAPE_MODEL": model,
            }
        ),
        encoding="utf-8",
    )
    path.chmod(stat.S_IRUSR | stat.S_IWUSR)
    return path


def write_task_list(directory: Path) -> Path:
    path = directory / "tasks.txt"
    path.write_text(
        "first-task\n\n# a comment, skipped\n  second-task  \n",
        encoding="utf-8",
    )
    return path


def run(
    args: list[str],
    key_file: Path | None = None,
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess:
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": os.environ.get("HOME", "/tmp"),
        # A stand-in that prints its argv. `command -v` finds it because an
        # absolute path is executable.
        "RUN_HARBOR_BIN": str(ECHO),
    }
    if key_file is not None:
        env["RUN_HARBOR_KEY_FILE"] = str(key_file)
    if extra_env:
        env.update(extra_env)
    return subprocess.run(
        # The runner's shebang is bash and it uses arrays. Invoking it with `sh`
        # is what let this check pass on macOS — whose /bin/sh happens to accept
        # arrays — and fail on Ubuntu, where /bin/sh is dash. The script is
        # executed the way a caller executes it: through its own shebang.
        [str(RUNNER), *args],
        capture_output=True,
        text=True,
        env=env,
        cwd=str(ROOT),
    )


def command_of(result: subprocess.CompletedProcess) -> list[str] | None:
    """The argv the script would exec, from a `--dry-run` echo."""
    for line in result.stdout.splitlines():
        if line.startswith(str(ECHO)):
            return shlex.split(line)
    return None


def has_flag(argv: list[str], flag: str) -> bool:
    return flag in argv


def host_cpu_count() -> str | None:
    """The same number the runner is meant to find, read independently of it.

    `getconf` first because the runner uses it, then `sysctl`: a check that
    only knew one of the two would call a correct Darwin fallback a failure.
    """
    for command in (["getconf", "_NPROCESSORS_ONLN"], ["sysctl", "-n", "hw.ncpu"]):
        try:
            out = subprocess.run(
                command, capture_output=True, text=True, timeout=10
            ).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            continue
        if out.isdigit() and int(out) >= 1:
            return out
    return None


def value_of(argv: list[str], flag: str) -> str | None:
    if flag not in argv:
        return None
    index = argv.index(flag)
    return argv[index + 1] if index + 1 < len(argv) else None


def check_codape_argv(tmp: Path, tasks: Path, binary: Path) -> None:
    result = run(
        ["codape", str(tasks), "--dry-run", "--version", "v0.2.3",
         "--concurrent", "3", "--max-turns", "250"],
    )
    argv = command_of(result)
    check(argv is not None, f"a codape dry run printed no command:\n{result.stderr}")
    if argv is None:
        return
    joined = " ".join(argv)

    check(
        "codape_harbor_agent:CodapeHarborAgent" in joined,
        "the codape arm must name the adapter as `module:Class`; a file path "
        "fails with No module named",
    )
    check(
        "terminal-bench/terminal-bench@4.1.0" in joined,
        "the dataset must be the 4.1 release ref, not a sha256 version",
    )
    check(
        "--agent-setup-timeout-multiplier" in argv,
        "the setup multiplier is what keeps a slow image download from timing "
        "out before the agent runs",
    )
    check(
        value_of(argv, "-n") == "3", "`--concurrent 3` must become `-n 3`"
    )
    # `-n` is an experimental parameter, not a speed knob: the same nine tasks
    # on the same host at `-n 2` and `-n 4` flip two results and move one
    # task's input by 22x. So it must be chained in the argv, not left to
    # Harbor's own default of 1, which no recorded run used.
    check(
        has_flag(argv, "-n"),
        "the runner must always pass `-n`, or Harbor's default of 1 silences "
        "the host the run was sized for",
    )
    # `--resources` has to be visible in the argv or the log, never guessed.
    # `auto` is Harbor's own default and is spelled by omitting the flags: the
    # 54-task baseline omitted them and its `config.json` recorded `auto`, so
    # passing `--cpus auto` would assume the CLI parses its own default as a
    # literal — an unsupported assumption whose cost is a paid run that never
    # starts.
    check(
        not has_flag(argv, "--cpus") and not has_flag(argv, "--memory"),
        "the default policy is `auto`, which is spelled by omitting the flags; "
        "`--cpus auto` is a literal the archived runs never used",
    )
    check(
        "--resources auto" in result.stderr,
        "the default policy must be announced, so a log records it",
    )
    ignored = run(
        ["codape", str(tasks), "--dry-run", "--resources", "ignore"],
    )
    ignored_argv = command_of(ignored)
    check(
        ignored_argv is not None
        and value_of(ignored_argv, "--cpus") == "ignore"
        and value_of(ignored_argv, "--memory") == "ignore",
        "`--resources ignore` must reach Harbor as both `--cpus ignore` and "
        "`--memory ignore`; `ignore` is what reproducing Part 1 needs",
    )
    check(
        run(["codape", str(tasks), "--dry-run", "--resources", "auto-ish"]).returncode
        != 0,
        "an unknown --resources value must be refused, not treated as `ignore`",
    )
    check(
        "max_turns=250" in joined,
        "`--max-turns` must reach the adapter as a kwarg",
    )
    check(
        "version=v0.2.3" in joined,
        "`--version` must reach the adapter, or the arm curls its default pin "
        "and measures a different release than the one asked for",
    )
    check(
        "-i" in argv and "terminal-bench/first-task" in argv
        and "terminal-bench/second-task" in argv,
        "every task in the list becomes one `-i` argument",
    )
    check(
        argv.count("-i") == 2,
        f"a blank line and a comment must not become tasks; got {argv.count('-i')} `-i`",
    )

    # The two pins are mutually exclusive at the adapter, which keeps an
    # explicit version beside a binary path. Passing both is the defect the
    # provenance fix exists for, so the runner must drop one.
    both = run(
        ["codape", str(tasks), "--dry-run", "--version", "v0.2.3",
         "--binary", str(binary)],
    )
    both_argv = command_of(both)
    check(
        both_argv is not None and not any(
            part.startswith("version=") for part in (both_argv or [])
        ),
        "`--version` beside `--binary` must be dropped, not passed: the adapter "
        "would stamp agent_info.version with a release the trial did not run",
    )
    check(
        both_argv is not None and f"binary_path={binary}" in both_argv,
        "`--binary` must reach the adapter as an absolute path",
    )
    check(
        "ignoring --version" in both.stderr,
        "dropping `--version` must say so rather than happen silently",
    )

    # A relative path is resolved before it reaches the adapter, because the
    # adapter resolves it against Harbor's working directory, not the caller's.
    relative = run(
        ["codape", str(tasks), "--dry-run", "--binary", "terminal-bench/run-harbor.sh"],
    )
    relative_argv = command_of(relative)
    check(
        relative_argv is not None
        and any(part.startswith(f"binary_path={ROOT}") for part in relative_argv),
        "a relative `--binary` must be made absolute",
    )

    # `--pricing` has the same trap and the same fix: the adapter reads it with
    # a plain `open` against Harbor's working directory. A relative path works
    # only when the caller stands in the repository root, and the failure is a
    # completed paid run whose result frames carry no `total_cost_usd`.
    relative_pricing = run(
        ["codape", str(tasks), "--dry-run", "--pricing", "terminal-bench/prices.json"],
    )
    pricing_argv = command_of(relative_pricing)
    check(
        pricing_argv is not None
        and any(
            part.startswith(f"pricing_path={ROOT}/terminal-bench/prices.json")
            for part in pricing_argv
        ),
        "a relative `--pricing` must be made absolute, or the adapter reads it "
        "against the wrong directory and the run reports no cost",
    )


def check_guards(tmp: Path, tasks: Path) -> None:
    # A model the gateway does not serve costs a container and twenty minutes to
    # discover mid-trial.
    bad_model = write_key_file(tmp, model="gpt-4")
    result = run(["codape", str(tasks)], key_file=bad_model)
    check(result.returncode != 0, "a model outside ALLOWED_MODELS must be refused")
    check(
        "does not serve" in result.stderr,
        f"the refusal must name the problem; got: {result.stderr.strip()!r}",
    )

    # A missing credential is refused before anything is spent.
    empty = tmp / "empty.json"
    empty.write_text('{"CODAPE_MODEL": "deepseek-v4.1-flash"}', encoding="utf-8")
    result = run(["codape", str(tasks)], key_file=empty)
    check(result.returncode != 0, "a key file missing a required name must be refused")
    check(
        "CODAPE_AUTH_TOKEN" in result.stderr,
        "the refusal must name the missing key, not the value",
    )

    # The pin that only applies to one arm must be refused on the other, not
    # silently ignored: an arm meant to measure a local binary that measured a
    # release instead is the failure this whole file is about.
    result = run(["claude", str(tasks), "--dry-run", "--version", "v0.2.3"])
    check(
        result.returncode != 0,
        "a Claude arm with --version must be refused rather than ignored",
    )

    # `auto` resolves from the host. The oracle is a *second* reading of the
    # host, taken here rather than derived from the script: comparing the
    # announced number against the argv alone would pass for any constant
    # (2, 4, 8) that happens to be smaller than this machine.
    auto = run(["codape", str(tasks), "--dry-run"])
    auto_argv = command_of(auto)
    announced = None
    for line in auto.stderr.splitlines():
        if "defaults to auto" in line:
            tail = line.rsplit("=", 1)[-1].strip().split()[0]
            announced = tail if tail.isdigit() else None
    check(
        announced is not None and int(announced) >= 1,
        f"`auto` must resolve to a positive integer; stderr was "
        f"{auto.stderr.strip()!r}",
    )
    check(
        auto_argv is not None and value_of(auto_argv, "-n") == announced,
        f"the resolved concurrency ({announced}) must be the one in the argv, "
        f"not a second reading of the host",
    )
    host = host_cpu_count()
    check(
        host is not None and announced == host,
        f"`auto` must be this host's CPU count ({host}), not a fixed number; "
        f"got {announced}. A constant is wrong in both directions: it wastes a "
        "larger host and oversubscribes a smaller one",
    )
    check(
        "defaults to auto" in auto.stderr,
        "the resolved concurrency must be printed, so a log says which one ran",
    )
    # An explicit `-n` wins over `auto`, and must not print the note.
    explicit = run(["codape", str(tasks), "--dry-run", "-n", "2"])
    explicit_argv = command_of(explicit)
    check(
        explicit_argv is not None and value_of(explicit_argv, "-n") == "2",
        "an explicit `--concurrent` must override `auto`",
    )
    check(
        "defaults to auto" not in explicit.stderr,
        "`auto` must not be resolved, or announced, when `-n` was given",
    )

    # A task list with no task names must be refused, not run as zero tasks.
    no_tasks = tmp / "none.txt"
    check(run(["nobody", str(tasks), "--dry-run"]).returncode != 0, "unknown arm")
    check(
        run(["codape", str(no_tasks), "--dry-run"]).returncode != 0,
        "a task list with no task names must be refused, not run as zero tasks",
    )
    check(
        run(["codape", str(tmp / "absent.txt"), "--dry-run"]).returncode != 0,
        "a missing task list must be refused",
    )


def check_no_leak(tmp: Path, tasks: Path) -> None:
    key_file = write_key_file(tmp)
    result = run(["codape", str(tasks), "--dry-run"], key_file=key_file)
    check(
        SENTINEL not in result.stdout and SENTINEL not in result.stderr,
        "the token must never appear in output, including --dry-run",
    )

    # And not in a child's argv either: a token on a command line is visible to
    # `ps`. The stand-in harbor prints the environment it received, so the same
    # sentinel appearing there is expected — argv is what must not carry it.
    probe = tmp / "probe.sh"
    probe.write_text(
        '#!/bin/sh\nfor arg in "$@"; do printf "ARG:%s\\n" "$arg"; done\n',
        encoding="utf-8",
    )
    probe.chmod(0o755)
    env = {"RUN_HARBOR_KEY_FILE": str(key_file), "RUN_HARBOR_BIN": str(probe)}
    result = subprocess.run(
        [str(RUNNER), "codape", str(tasks)],
        capture_output=True,
        text=True,
        env={**os.environ, **env},
    )
    check(
        SENTINEL not in result.stdout,
        "the token reached the harbor command line, where `ps` would show it",
    )


def check_hostile_values(tmp: Path, tasks: Path) -> None:
    """A value with a quote or a space must be exported intact, not word-split.

    `shlex.quote` is the only thing between a key file and an `eval`. A value
    without a metacharacter in it passes whether or not the quoting is there, so
    this uses one that does not: dropped quoting turns `A B" C` into a syntax
    error or, worse, a truncated token that fails at the gateway after a
    container is built.
    """
    # Hostile to a shell, harmless to a filesystem: quotes break the eval, a
    # space splits an argument, and `$(...)` would execute if the value were
    # ever re-evaluated.
    hostile = 'it s a "quoted" $(echo pwned) value'
    path = tmp / "hostile.json"
    path.write_text(
        json.dumps(
            {
                "CODAPE_AUTH_TOKEN": hostile,
                "CODAPE_BASE_URL": "https://gateway.invalid/v1",
                "CODAPE_MODEL": "deepseek-v4.1-flash",
            }
        ),
        encoding="utf-8",
    )
    path.chmod(stat.S_IRUSR | stat.S_IWUSR)

    probe = tmp / "receive.sh"
    probe.write_text(
        "#!/bin/sh\nprintf 'GOT:[%s]\\n' \"$CODAPE_AUTH_TOKEN\"\n",
        encoding="utf-8",
    )
    probe.chmod(0o755)
    result = subprocess.run(
        [str(RUNNER), "codape", str(tasks)],
        capture_output=True,
        text=True,
        env={
            **os.environ,
            "RUN_HARBOR_KEY_FILE": str(path),
            "RUN_HARBOR_BIN": str(probe),
        },
    )
    check(
        f"GOT:[{hostile}]" in result.stdout,
        "a token containing a quote, a space and a semicolon must reach the "
        f"child intact; got {result.stdout.strip()!r} / {result.stderr.strip()!r}",
    )


def check_shipped_lists() -> None:
    for name, expected in (
        ("tasks-control9.txt", 9),
        ("tasks-clean10.txt", 10),
        ("tasks-capability2.txt", 2),
        ("tasks-all54.txt", 54),
    ):
        path = TASK_LISTS / name
        check(path.is_file(), f"{name} is missing; a re-run would have no task set")
        if not path.is_file():
            continue
        names = [
            line.strip()
            for line in path.read_text(encoding="utf-8").splitlines()
            if line.strip() and not line.strip().startswith("#")
        ]
        check(
            len(names) == expected,
            f"{name} should hold {expected} tasks, holds {len(names)}: the "
            "recorded comparisons are built on exactly this set",
        )
        check(
            len(set(names)) == len(names),
            f"{name} repeats a task name",
        )


def check_pricing() -> None:
    # The adapter fails at construction on a malformed table, but only after
    # Harbor starts; a bad file here is a paid run that reports no cost at all.
    path = TASK_LISTS / "prices.json"
    check(path.is_file(), "prices.json is missing; trials would report no cost")
    if not path.is_file():
        return
    try:
        table = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        check(False, f"prices.json is not JSON: {exc}")
        return
    check(isinstance(table, dict) and table, "prices.json must map model ids to rates")
    for model, rates in table.items():
        for key in ("input_per_million", "output_per_million",
                    "cache_read_per_million", "cache_creation_per_million"):
            check(
                isinstance(rates, dict) and isinstance(rates.get(key), (int, float)),
                f"prices.json[{model}] lacks a numeric {key}",
            )


ECHO = Path("/bin/echo")


def main() -> int:
    if not RUNNER.is_file():
        print(f"error: {RUNNER} is missing", file=sys.stderr)
        return 1

    with tempfile.TemporaryDirectory() as raw:
        tmp = Path(raw)
        global ECHO
        ECHO = tmp / "harbor-echo"
        ECHO.write_text("#!/bin/sh\nexec /bin/echo \"$@\"\n", encoding="utf-8")
        ECHO.chmod(0o755)

        tasks = write_task_list(tmp)
        binary = tmp / "codape"
        binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        binary.chmod(0o755)

        check_codape_argv(tmp, tasks, binary)
        check_guards(tmp, tasks)
        check_no_leak(tmp, tasks)
        check_hostile_values(tmp, tasks)

    check_shipped_lists()
    check_pricing()

    if FAILURES:
        for failure in FAILURES:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print("harbor runner: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
