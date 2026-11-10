#!/usr/bin/env python3
"""Run Codape on one ProgramBench instance and grade it with the official harness.

ProgramBench gives the agent a compiled reference executable and its bundled
documentation, and asks for an independent source reimplementation plus a
``compile.sh`` that produces ``./executable``.  The official package grades the
submitted workspace in a Docker container.

This wrapper adds the Codape-specific parts: it installs a pinned Codape
release, starts the cleanroom image, runs the headless agent, extracts the
workspace, and hands it to ``programbench eval``.

The task container is attached to an internal Docker network.  A small
allowlisted CONNECT proxy is its only route out, and it permits only
``opencode.ai:443`` so Codape can reach the model while the task itself has no
general internet.  This is the ProgramBench inference rule expressed as
network policy, not just as prompt text.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import time
import urllib.request
import uuid
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]

PROGRAMBENCH_VERSION = "1.2.5"
CODAPE_VERSION = "v0.2.0"
CODAPE_TARGET = "x86_64-unknown-linux-musl"
CLEANROOM_TAG = "task_cleanroom_v6"
DEFAULT_INSTANCE = "abishekvashok__cmatrix.5c082c6"
DEFAULT_TIMEOUT = 5400
DEFAULT_MAX_TURNS = 400
DEFAULT_CPUS = 8
DEFAULT_MEMORY = "8g"
INTERNAL_NETWORK = "codape-programbench-internal"
PROXY_CONTAINER = "codape-programbench-proxy"
PROXY_PORT = 8888
PROXY_ALLOWED_HOST = "opencode.ai"

PASSTHROUGH_ENV = (
    "CODAPE_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "CODAPE_BASE_URL",
    "ANTHROPIC_BASE_URL",
    "OPENAI_BASE_URL",
    "CODAPE_MODEL",
    "ANTHROPIC_MODEL",
    "OPENAI_MODEL",
    "CODAPE_CUSTOM_HEADERS",
    "CODAPE_CONTEXT_WINDOW",
    "CODAPE_MAX_TOKENS",
)

INSTRUCTION = """\
You are working in /workspace on a ProgramBench task.

## Task

Recreate the program from scratch. The only specification is the reference
executable at ./executable and the documentation files in the workspace. Test
the executable through its normal command-line interface and write a complete,
independent implementation.

## Rules

- Do NOT search the internet, clone repositories, or download this project from
  any package registry.
- Do NOT decompile, disassemble, or trace the reference executable. You may run
  it with any arguments and observe its input/output.
- Do NOT wrap, copy, or delegate to the reference executable. The evaluator
  deletes ./executable before building your submission, so a wrapper cannot
  work.

## Deliverable

- Write the source code in /workspace.
- Write an executable ./compile.sh that builds ./executable from your source.
  It must work in a fresh checkout of /workspace with ./executable removed.
- Keep generated binaries out of the source if you use git; a git commit is not
  required for submission.
- Check which build tools the cleanroom provides (it may have gcc, make,
  cmake, python3, cargo/rustc, git, curl, and wget). There is no
  package-registry or general internet access, so do not depend on downloading
  libraries or build dependencies.
- When you are done, stop. The workspace is submitted automatically.

Pace yourself: do not spend more than about 20 minutes probing before writing a
first complete version, and keep at least the last quarter of the time budget
for building and final checks. A working build with a few rough edges beats a
perfect probe with no submission.

Work methodically: read the docs, probe the executable, write the
implementation, build it with your compile.sh, and compare its observable
behavior with the reference before you finish.
"""

PROXY_SOURCE = r"""
import select
import socket
import threading

LISTEN = ("0.0.0.0", 8888)
ALLOWED_HOST = "opencode.ai"
ALLOWED_PORT = 443


def pump(client, upstream):
    sockets = [client, upstream]
    try:
        while True:
            readable, _, _ = select.select(sockets, [], [], 60)
            if not readable:
                return
            for source in readable:
                data = source.recv(65536)
                if not data:
                    return
                (upstream if source is client else client).sendall(data)
    finally:
        client.close()
        upstream.close()


def handle(client):
    upstream = None
    try:
        stream = client.makefile("rb")
        request_line = stream.readline(65536)
        if not request_line:
            return
        parts = request_line.decode("latin-1").split()
        if len(parts) < 2 or parts[0] != "CONNECT":
            client.sendall(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            return
        target = parts[1]
        host, _, port_text = target.rpartition(":")
        port = int(port_text)
        if not (host == ALLOWED_HOST or host.endswith("." + ALLOWED_HOST)) or port != ALLOWED_PORT:
            client.sendall(b"HTTP/1.1 403 Forbidden\r\n\r\n")
            return
        while True:
            line = stream.readline(65536)
            if line in (b"\r\n", b"\n", b""):
                break
        upstream = socket.create_connection((host, port), timeout=30)
        client.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        pump(client, upstream)
    except Exception:
        try:
            client.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
        except Exception:
            pass
    finally:
        if upstream is not None:
            try:
                upstream.close()
            except Exception:
                pass
        try:
            client.close()
        except Exception:
            pass


def main():
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(LISTEN)
    server.listen(100)
    while True:
        conn, _ = server.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
"""


class BenchError(RuntimeError):
    pass


def log(message: str) -> None:
    print(message, flush=True)


def run(command: list[str], *, timeout: int | None = None) -> subprocess.CompletedProcess:
    log("+ " + " ".join(command))
    return subprocess.run(command, check=False, timeout=timeout)


def check(command: list[str], *, timeout: int | None = None) -> subprocess.CompletedProcess:
    result = run(command, timeout=timeout)
    if result.returncode != 0:
        raise BenchError(f"command failed ({result.returncode}): {' '.join(command)}")
    return result


def output(command: list[str], *, timeout: int | None = None) -> str:
    result = subprocess.run(command, check=True, text=True, capture_output=True, timeout=timeout)
    return result.stdout.strip()


def docker_available() -> None:
    if shutil.which("docker") is None:
        raise BenchError("docker CLI not found on PATH")
    if run(["docker", "info"], timeout=30).returncode != 0:
        raise BenchError("docker daemon is not reachable")


def cache_root() -> Path:
    explicit = os.environ.get("CODAPE_BENCH_CACHE")
    if explicit:
        return Path(explicit).expanduser().resolve()
    return Path.home() / ".cache" / "codape-external-benchmarks"


def programbench_venv() -> Path:
    return cache_root() / f"venv-programbench-{PROGRAMBENCH_VERSION}"


def programbench_python() -> Path:
    return programbench_venv() / "bin" / "python"


def programbench_bin() -> Path:
    return programbench_venv() / "bin" / "programbench"


def ensure_programbench() -> None:
    python = programbench_python()
    binary = programbench_bin()
    probe = (
        "from importlib.metadata import version\n"
        f"assert version('programbench') == '{PROGRAMBENCH_VERSION}', version('programbench')\n"
        "import programbench\n"
    )
    if python.exists() and binary.exists():
        result = subprocess.run([str(python), "-I", "-c", probe], capture_output=True, text=True)
        if result.returncode == 0:
            return
    programbench_venv().parent.mkdir(parents=True, exist_ok=True)
    log(f"creating ProgramBench venv in {programbench_venv()}")
    check([sys.executable, "-m", "venv", str(programbench_venv())])
    check([str(python), "-m", "pip", "install", "--upgrade", "pip"])
    check([str(python), "-m", "pip", "install", f"programbench=={PROGRAMBENCH_VERSION}"])


def codape_binary() -> Path:
    root = cache_root() / f"codape-{CODAPE_VERSION}"
    binary = root / f"codape-{CODAPE_TARGET}"
    if binary.is_file():
        return binary
    # A previous extraction may have left a directory of the same name when
    # the archive member and the target path collided.
    if binary.exists():
        if binary.is_dir():
            shutil.rmtree(binary)
        else:
            binary.unlink()
    root.mkdir(parents=True, exist_ok=True)
    archive = root / f"codape-{CODAPE_TARGET}.tar.gz"
    sums = root / "SHA256SUMS"
    base = f"https://github.com/lightconsen/codape/releases/download/{CODAPE_VERSION}"
    if not archive.exists():
        log(f"downloading {base}/{archive.name}")
        urllib.request.urlretrieve(f"{base}/{archive.name}", archive)
    if not sums.exists():
        log(f"downloading {base}/SHA256SUMS")
        urllib.request.urlretrieve(f"{base}/SHA256SUMS", sums)
    expected = None
    for line in sums.read_text().splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[1] == archive.name:
            expected = fields[0]
            break
    if not expected:
        raise BenchError(f"SHA256SUMS has no entry for {archive.name}")
    actual = hashlib.sha256(archive.read_bytes()).hexdigest()
    if actual != expected:
        raise BenchError(f"checksum mismatch for {archive.name}: expected {expected}, got {actual}")
    with tempfile.TemporaryDirectory(dir=root) as tmp:
        check(["tar", "-xzf", str(archive), "-C", tmp])
        extracted = Path(tmp) / binary.name
        if not extracted.is_file():
            raise BenchError(f"release archive did not contain {binary.name}")
        shutil.move(str(extracted), binary)
    binary.chmod(0o755)
    return binary


def load_instance(instance_id: str) -> dict:
    python = programbench_python()
    code = textwrap.dedent(
        """
        import json
        from programbench.utils.load_data import load_all_instances
        wanted = %r
        for item in load_all_instances(include_tests=False):
            if item["instance_id"] == wanted:
                print(json.dumps(item))
                break
        else:
            raise SystemExit("instance not found: " + wanted)
        """
    ) % instance_id
    result = subprocess.run([str(python), "-I", "-c", code], capture_output=True, text=True)
    if result.returncode != 0:
        raise BenchError(result.stderr.strip() or f"could not load instance {instance_id}")
    return json.loads(result.stdout)


def image_for(instance_id: str) -> str:
    return f"programbench/{instance_id.replace('__', '_1776_')}:{CLEANROOM_TAG}"


def ensure_internal_network() -> None:
    if run(["docker", "network", "inspect", INTERNAL_NETWORK], timeout=30).returncode == 0:
        return
    check(["docker", "network", "create", "--internal", INTERNAL_NETWORK])


def stop_proxy() -> None:
    run(["docker", "rm", "-f", PROXY_CONTAINER], timeout=60)


def start_proxy() -> None:
    stop_proxy()
    handle = tempfile.NamedTemporaryFile("w", suffix=".py", delete=False, encoding="utf-8")
    try:
        handle.write(PROXY_SOURCE)
        handle.close()
        check(["docker", "image", "inspect", "python:3.12-slim"], timeout=30)
    except BenchError:
        check(["docker", "pull", "python:3.12-slim"], timeout=300)
    try:
        check(
            [
                "docker",
                "run",
                "-d",
                "--name",
                PROXY_CONTAINER,
                "--network",
                INTERNAL_NETWORK,
                "-v",
                f"{handle.name}:/proxy.py:ro",
                "python:3.12-slim",
                "python",
                "/proxy.py",
            ],
            timeout=60,
        )
        # The proxy needs one leg on a network with a route out; the task
        # container only ever sees the internal leg.
        check(["docker", "network", "connect", "bridge", PROXY_CONTAINER], timeout=60)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            probe = run(
                [
                    "docker",
                    "exec",
                    PROXY_CONTAINER,
                    "python",
                    "-c",
                    "import socket; socket.create_connection(('127.0.0.1', 8888), 2).close()",
                ],
                timeout=10,
            )
            if probe.returncode == 0:
                return
            time.sleep(1)
        raise BenchError("the ProgramBench model proxy did not start listening")
    finally:
        Path(handle.name).unlink(missing_ok=True)


def container_env() -> list[str]:
    args: list[str] = []
    for name in PASSTHROUGH_ENV:
        if os.environ.get(name):
            args += ["-e", name]
    # These are not secrets; they tell Codape to use the allowlisted proxy and
    # keep its own state out of the submitted workspace.
    args += [
        "-e",
        "CODAPE_SANDBOX=off",
        "-e",
        "CODAPE_LOG=off",
        "-e",
        "NO_COLOR=1",
        "-e",
        "CLAUDE_CONFIG_DIR=/tmp/codape-config",
        "-e",
        f"HTTPS_PROXY=http://{PROXY_CONTAINER}:{PROXY_PORT}",
        "-e",
        f"HTTP_PROXY=http://{PROXY_CONTAINER}:{PROXY_PORT}",
        "-e",
        f"ALL_PROXY=http://{PROXY_CONTAINER}:{PROXY_PORT}",
        "-e",
        "NO_PROXY=localhost,127.0.0.1",
    ]
    return args


def start_container(image: str, name: str, cpus: int, memory: str, lifetime: int) -> None:
    # The container's PID 1 must outlive the agent timeout.  With ``sleep 2h``
    # and a two-hour agent budget the two expire together, the agent's exec is
    # killed before it writes its exit code, and the workspace can be lost
    # before submission extraction.  ``sleep infinity`` leaves cleanup to the
    # runner's explicit ``docker rm -f``.
    check(
        [
            "docker",
            "run",
            "-d",
            "--init",
            "--name",
            name,
            "--network",
            INTERNAL_NETWORK,
            "--user",
            "agent",
            "-w",
            "/workspace",
            "--cpus",
            str(cpus),
            "--memory",
            memory,
            "--memory-swap",
            memory,
            "--cap-drop",
            "SYS_PTRACE",
            image,
            "sleep",
            "infinity",
        ],
        timeout=120,
    )
    log(f"container lifetime: infinity (agent budget {lifetime}s)")


def ensure_running(container: str) -> None:
    """Restart a stopped task container so the workspace can still be read."""
    probe = subprocess.run(
        ["docker", "inspect", "-f", "{{.State.Running}}", container],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if probe.returncode == 0 and probe.stdout.strip() == "true":
        return
    log(f"container {container} is not running; restarting it to extract the submission")
    check(["docker", "start", container], timeout=60)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        probe = subprocess.run(
            ["docker", "inspect", "-f", "{{.State.Running}}", container],
            capture_output=True,
            text=True,
            timeout=30,
        )
        if probe.returncode == 0 and probe.stdout.strip() == "true":
            return
        time.sleep(1)
    raise BenchError(f"container {container} could not be restarted")


def copy_text(container: str, text: str, destination: str) -> None:
    handle = tempfile.NamedTemporaryFile("w", delete=False, encoding="utf-8")
    try:
        handle.write(text)
        handle.close()
        check(["docker", "cp", handle.name, f"{container}:{destination}"], timeout=60)
    finally:
        Path(handle.name).unlink(missing_ok=True)


def read_file(container: str, source: str, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    check(["docker", "cp", f"{container}:{source}", str(destination)], timeout=120)


def run_agent(container: str, timeout: int, max_turns: int) -> int:
    script = (
        f"timeout --signal=TERM --kill-after=30 {timeout} "
        "/tmp/codape -p --dangerously-skip-permissions --no-session-persistence "
        f"--no-telemetry --output-format stream-json --max-turns {max_turns} "
        "< /tmp/programbench-instruction.txt "
        "> /tmp/codape-stream.jsonl 2>&1; "
        "printf '%s\\n' $? > /tmp/codape-exit-code"
    )
    command = ["docker", "exec", "-u", "agent", "-w", "/workspace", *container_env(), container, "sh", "-c", script]
    run(command, timeout=timeout + 180)
    result = subprocess.run(
        ["docker", "exec", container, "cat", "/tmp/codape-exit-code"],
        capture_output=True,
        text=True,
        timeout=30,
    )
    try:
        return int(result.stdout.strip())
    except ValueError:
        return -1


def summarize_stream(path: Path) -> str:
    if not path.exists():
        return "stream missing"
    lines = [line for line in path.read_text(errors="replace").splitlines() if line.strip()]
    if not lines:
        return "stream empty"
    frames = []
    for line in lines:
        try:
            frames.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    results = [frame for frame in frames if frame.get("type") == "result"]
    if results:
        # Codape emits one result frame per continuation segment; summing them
        # is the only way to recover the whole run's turns and wall clock.
        turns = sum(frame.get("num_turns") or 0 for frame in results)
        duration = sum(frame.get("duration_ms") or 0 for frame in results)
        last = results[-1]
        return (
            f"result frames={len(results)} subtype={last.get('subtype', '?')} "
            f"is_error={any(frame.get('is_error') for frame in results)} "
            f"turns={turns} duration_ms={duration}"
        )
    if not frames:
        return f"no JSON frames; last line: {lines[-1][:400]}"
    last = frames[-1]
    return f"no result frame; last frame type={last.get('type', '?')}"


def parse_eval(path: Path) -> dict:
    data = json.loads(path.read_text())
    results = data.get("test_results", [])
    passed = sum(1 for result in results if result.get("status") == "passed")
    return {
        "passed": passed,
        "total": len(results),
        "error_code": data.get("error_code"),
        "branch_errors": data.get("test_branch_errors") or {},
        "warnings": len(data.get("warnings") or []),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run Codape on one ProgramBench instance")
    parser.add_argument("--instance", default=DEFAULT_INSTANCE, help=f"ProgramBench instance ID (default: {DEFAULT_INSTANCE})")
    parser.add_argument("--output-dir", type=Path, default=REPO_ROOT / "target" / "external-benchmarks" / "programbench")
    parser.add_argument("--run-id", help="Run identifier (default: pb-YYYYmmdd-HHMMSS)")
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT, help="Agent timeout in seconds")
    parser.add_argument("--max-turns", type=int, default=DEFAULT_MAX_TURNS)
    parser.add_argument("--cpus", type=int, default=DEFAULT_CPUS)
    parser.add_argument("--memory", default=DEFAULT_MEMORY)
    parser.add_argument("--model", help="Model ID passed to Codape")
    parser.add_argument("--keep-container", action="store_true", help="Leave the task container running")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)

    if args.run_id is None:
        args.run_id = "pb-" + time.strftime("%Y%m%d-%H%M%S", time.gmtime())
    if not re.fullmatch(r"[A-Za-z0-9._-]+", args.run_id):
        parser.error("--run-id may contain only letters, digits, dot, underscore, and hyphen")
    args.output_dir = args.output_dir.expanduser().resolve()

    if args.dry_run:
        print("ProgramBench dry run; nothing will be downloaded or executed.")
        print(f"instance      : {args.instance}")
        print(f"package       : programbench=={PROGRAMBENCH_VERSION}")
        print(f"codape        : {CODAPE_VERSION} ({CODAPE_TARGET})")
        print(f"cleanroom     : {image_for(args.instance)}")
        print(f"network       : internal {INTERNAL_NETWORK} via allowlisted proxy {PROXY_ALLOWED_HOST}:443")
        print(f"output        : {args.output_dir / args.run_id}")
        print(f"timeout       : {args.timeout}s, max turns {args.max_turns}")
        return 0

    try:
        docker_available()
        ensure_programbench()
        ensure_internal_network()
        instance = load_instance(args.instance)
        image = image_for(args.instance)
        log(f"instance: {args.instance} ({instance.get('language', '?')}, {instance.get('difficulty', '?')})")
        check(["docker", "pull", image], timeout=1800)
        binary = codape_binary()

        run_dir = args.output_dir / args.run_id
        instance_dir = run_dir / args.instance
        instance_dir.mkdir(parents=True, exist_ok=True)
        container = f"pb-codape-{uuid.uuid4().hex[:10]}"

        start_proxy()
        try:
            start_container(image, container, args.cpus, args.memory, args.timeout)
            check(["docker", "cp", str(binary), f"{container}:/tmp/codape"], timeout=60)
            check(["docker", "exec", "-u", "0", container, "chmod", "0755", "/tmp/codape"], timeout=30)
            copy_text(
                container,
                '{"config_version": 4, "telemetry_enabled": false}\n',
                "/tmp/codape-settings.json",
            )
            check(
                [
                    "docker",
                    "exec",
                    "-u",
                    "agent",
                    container,
                    "sh",
                    "-c",
                    "mkdir -p /tmp/codape-config && cp /tmp/codape-settings.json /tmp/codape-config/settings.json",
                ],
                timeout=30,
            )
            copy_text(container, INSTRUCTION, "/tmp/programbench-instruction.txt")

            log(f"agent: {args.timeout}s, {args.max_turns} turns, container {container}")
            exit_code = run_agent(container, args.timeout, args.max_turns)
            log(f"agent exit code: {exit_code}")
            if exit_code != 0:
                log("warning: the agent did not exit cleanly; still extracting the workspace")

            # A stopped container keeps its writable layer, so restarting it
            # recovers the workspace even when the agent was killed at its
            # timeout.  Do this before reading the stream and the submission.
            ensure_running(container)

            stream_path = instance_dir / "codape-stream.jsonl"
            try:
                read_file(container, "/tmp/codape-stream.jsonl", stream_path)
            except BenchError as exc:
                log(f"warning: could not copy the agent stream: {exc}")
            log(summarize_stream(stream_path))

            # The reference executable is deliberately execute-only (no read
            # permission) so the agent cannot copy or disassemble it.  It is
            # also removed by the grader before compiling, so leave it out of
            # the submission rather than fail the whole tar on EACCES.
            check(
                [
                    "docker",
                    "exec",
                    "-u",
                    "agent",
                    "-w",
                    "/workspace",
                    container,
                    "tar",
                    "-czf",
                    "/tmp/submission.tar.gz",
                    "--exclude=./executable",
                    "-C",
                    "/workspace",
                    ".",
                ],
                timeout=300,
            )
            read_file(container, "/tmp/submission.tar.gz", instance_dir / "submission.tar.gz")
            log(f"submission: {instance_dir / 'submission.tar.gz'}")
        finally:
            if not args.keep_container:
                run(["docker", "rm", "-f", container], timeout=60)
            stop_proxy()

        log("evaluating with the official ProgramBench harness")
        eval_log = instance_dir / "eval.log"
        with eval_log.open("w") as handle:
            result = subprocess.run(
                [
                    str(programbench_bin()),
                    "eval",
                    str(run_dir),
                    "--workers",
                    "1",
                    "--branch-workers",
                    "1",
                    "--docker-cpus",
                    str(args.cpus),
                ],
                stdout=handle,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=6 * 3600,
            )
        eval_json = instance_dir / f"{args.instance}.eval.json"
        summary = parse_eval(eval_json) if eval_json.exists() else None
        print(f"eval exit code: {result.returncode}")
        print(f"eval log: {eval_log}")
        if summary is None:
            print(f"PROGRAMBENCH_SUMMARY run_id={args.run_id} instance={args.instance} score=na")
            return 1
        # `programbench info` applies the task's ignored tests/branches, which
        # the raw eval JSON intentionally keeps. Print it so a reported score
        # can use the official filtering rather than every raw testcase.
        info = subprocess.run(
            [str(programbench_bin()), "info", str(run_dir)],
            capture_output=True,
            text=True,
            timeout=300,
        )
        if info.returncode == 0:
            print(info.stdout)
        else:
            print(f"info failed ({info.returncode}): {info.stderr.strip()}")
        print(
            f"PROGRAMBENCH_SUMMARY run_id={args.run_id} instance={args.instance} "
            f"raw_passed={summary['passed']} raw_total={summary['total']} "
            f"raw_score={summary['passed'] / summary['total'] if summary['total'] else 0:.4f} "
            f"error_code={summary['error_code']} warnings={summary['warnings']}"
        )
        if summary["branch_errors"]:
            print(f"branch errors: {summary['branch_errors']}")
        return 0 if result.returncode == 0 else 1
    except BenchError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
