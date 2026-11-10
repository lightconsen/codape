"""Terminal-Bench adapter that runs the Codape CLI inside the task container.

Terminal-Bench installs an agent into the task's Docker container and then
drives a tmux session in that container. This adapter follows the same shape as
the built-in installed agents:

* ``codape-setup.sh.j2`` downloads or copies a pinned Codape release.
* ``_run_agent_commands`` runs ``codape -p`` headlessly in that tmux session.
* ``perform_task`` copies the task instruction into the container as a file so
  that newlines in the instruction cannot be interpreted by tmux as early
  Enter presses.

The harness evaluates container state after the agent exits; there is no
"answer" string to return. The adapter also records Codape's exit status and,
when the CLI emits it, stream-json token usage in the ``AgentResult``.

Usage with ``tb`` (see run_terminal_bench.py for the wrapper):

    tb run \
      --dataset terminal-bench-core==0.1.1 \
      --agent-import-path codape_agent:CodapeAgent \
      --agent-kwarg version=v0.2.0 \
      --model claude-sonnet-4-5-20250929
"""

from __future__ import annotations

import json
import os
import shlex
import tempfile
from pathlib import Path
from urllib.parse import urlsplit

from terminal_bench.agents.base_agent import AgentResult
from terminal_bench.agents.failure_mode import FailureMode
from terminal_bench.agents.installed_agents.abstract_installed_agent import (
    AbstractInstalledAgent,
)
from terminal_bench.terminal.models import TerminalCommand

# Fixed paths inside the task container. /installed-agent is where
# AbstractInstalledAgent puts the setup script and is mounted back to the host.
CODAPE_BIN = "/usr/local/bin/codape"
INSTALLED_DIR = "/installed-agent"
INSTRUCTION_PATH = f"{INSTALLED_DIR}/instruction.txt"
STREAM_PATH = f"{INSTALLED_DIR}/codape-stream.jsonl"
EXIT_CODE_PATH = f"{INSTALLED_DIR}/codape-exit-code"
CONFIG_DIR = "/tmp/codape-config"
LOCAL_BINARY_PATH = "/tmp/codape-local-binary"
# The harness mounts /logs back to the host's trial directory, so the full
# transcript is copied there before the container is removed.
LOGS_DIR = "/logs"
LOGS_STREAM_PATH = f"{LOGS_DIR}/codape-stream.jsonl"

# Hosts that must bypass the optional task proxy when one is configured.
# `host.docker.internal` because a container on Linux may need to reach the
# Docker host directly; the rest because a task often starts a service on the
# container itself and polls it over loopback.
NO_PROXY = "localhost,127.0.0.1,::1,host.docker.internal"

DEFAULT_VERSION = "v0.2.0"
DEFAULT_MAX_TURNS = 200
DEFAULT_TIMEOUT_SEC = 1800

# Host variables forwarded into the container. Codape itself applies this
# precedence; copying the whole set keeps the adapter out of the way when a
# user configures only one provider. Values come from the host environment and
# are never printed by this module.
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
    # Codape assumes a conservative 32K window for a model it does not know,
    # and compacts against that guess. A model with a larger real window then
    # spends most of the task compacting instead of working. These let the
    # caller state the window the endpoint actually has.
    "CODAPE_CONTEXT_WINDOW",
    # Raises the per-request output ceiling, which a reasoning model can
    # otherwise spend entirely on thinking and end the run having emitted
    # nothing. A raised value is clamped to half the model's context window.
    "CODAPE_MAX_TOKENS",
)


class CodapeAgent(AbstractInstalledAgent):
    """Installed agent that drives the Codape headless CLI."""

    def __init__(
        self,
        model_name: str | None = None,
        version: str = DEFAULT_VERSION,
        binary_path: str | None = None,
        max_turns: int | str = DEFAULT_MAX_TURNS,
        timeout_sec: float | str = DEFAULT_TIMEOUT_SEC,
        extra_args: str | list[str] | None = None,
        extra_instructions: str | None = None,
        task_proxy: str | None = None,
        task_proxy_direct_hosts: str | list[str] | None = None,
        **kwargs,
    ):
        super().__init__(**kwargs)
        self._model_name = str(model_name).removeprefix("anthropic/") if model_name else None
        self._version = str(version)
        self._binary_path = Path(str(binary_path)).expanduser() if binary_path else None
        self._max_turns = int(max_turns)
        self._timeout_sec = float(timeout_sec)
        self._extra_instructions = (
            str(extra_instructions).strip() if extra_instructions else None
        )
        # Optional proxy for everything the task container fetches — package
        # installs, dataset downloads, the model endpoint. On this machine the
        # container's own route to huggingface.co and youtube.com is blocked
        # while the host has a working proxy, and without this such tasks
        # cannot start. `localhost` is excluded so a task's own service on
        # port 5000 is never sent through it.
        self._task_proxy = str(task_proxy) if task_proxy else None
        # Hosts that should keep going direct even though a task proxy is set.
        # A proxy is not free: on this machine the same toolchain apt install
        # takes 66 s direct and over 20 minutes through the proxy, while only a
        # few hosts (huggingface.co, youtube.com) actually need it. A task that
        # installs packages *and* downloads from one of those needs both
        # halves, and this is how it gets them.
        if isinstance(task_proxy_direct_hosts, str):
            direct = task_proxy_direct_hosts.split(",")
        elif task_proxy_direct_hosts is None:
            direct = []
        else:
            direct = [str(host) for host in task_proxy_direct_hosts]
        self._task_proxy_direct_hosts = [
            host.strip() for host in direct if str(host).strip()
        ]
        if isinstance(extra_args, str):
            self._extra_args = shlex.split(extra_args)
        elif extra_args is None:
            self._extra_args = []
        else:
            self._extra_args = [str(arg) for arg in extra_args]

        if self._binary_path is not None and not self._binary_path.exists():
            raise FileNotFoundError(f"Codape binary does not exist: {self._binary_path}")

        # `/installed-agent` is the directory AbstractInstalledAgent mounts
        # back to the host, so the transcript and exit code survive the
        # container. This used to switch to a `/agent-logs` mount when the
        # harness supplied a logging directory, but that directory is not
        # created in terminal-bench 0.2.18 task containers: the redirect
        # failed with "No such file or directory" before Codape ever ran.
        self._stream_path = STREAM_PATH
        self._exit_code_path = EXIT_CODE_PATH

    @staticmethod
    def name() -> str:
        return "codape"

    @property
    def _env(self) -> dict[str, str]:
        env = {
            name: value
            for name in PASSTHROUGH_ENV
            if (value := os.environ.get(name))
        }
        if self._model_name:
            env["CODAPE_MODEL"] = self._model_name
        # Keep Codape's state out of the task's working tree and skip the
        # first-run telemetry consent prompt. CODAPE_LOG=off keeps the tmux
        # pane readable.
        env["CLAUDE_CONFIG_DIR"] = CONFIG_DIR
        env["CODAPE_LOG"] = "off"
        env["NO_COLOR"] = "1"
        # The task container is the isolation boundary. Codape's shell sandbox
        # denies network and the privilege-drop syscalls apt needs, so package
        # installation inside the bench would be impossible with it on.
        env["CODAPE_SANDBOX"] = "off"
        return env

    def _create_env_setup_file(self) -> str:
        # Same purpose as the base implementation, but shlex-quoted so a value
        # containing a single quote or shell metacharacter cannot break the
        # setup line or leak into the terminal.
        lines = [
            f"export {key}={shlex.quote(value)}" for key, value in self._env.items()
        ]
        if self._task_proxy:
            proxy = shlex.quote(self._task_proxy)
            lines.extend(
                [
                    f"export http_proxy={proxy}",
                    f"export https_proxy={proxy}",
                    f"export HTTP_PROXY={proxy}",
                    f"export HTTPS_PROXY={proxy}",
                    # A task that starts its own service and polls it must not
                    # send that request through the proxy, and neither must the
                    # model call — the provider endpoint is reachable directly,
                    # and a proxy that has not seen the key rejects it.
                    f"export no_proxy={shlex.quote(self._no_proxy_value())}",
                    f"export NO_PROXY={shlex.quote(self._no_proxy_value())}",
                ]
            )
        return "\n".join(lines)

    def _no_proxy_value(self) -> str:
        hosts = [NO_PROXY, *self._task_proxy_direct_hosts]
        for name in ("ANTHROPIC_BASE_URL", "CODAPE_BASE_URL", "OPENAI_BASE_URL"):
            value = os.environ.get(name)
            if not value:
                continue
            host = urlsplit(value).hostname
            if host:
                hosts.append(host)
        return ",".join(hosts)

    def _get_template_variables(self) -> dict[str, object]:
        return {
            "version": self._version,
            "target": "x86_64-unknown-linux-musl",
            "use_local_binary": self._binary_path is not None,
        }

    @property
    def _install_agent_script_path(self) -> Path:
        return self._get_templated_script_path("codape-setup.sh.j2")

    def _install_task_proxy(self, session) -> None:
        """Install the task proxy for every shell in the container.

        Exporting it in the agent's own environment is not enough: the tests
        run in a *separate* tmux session created by the harness, and several
        task scripts install their tooling there (the `uv` bootstrap is the
        common one). A profile fragment is read by every login shell the
        harness starts, so it covers the test phase too.

        The provider host stays out of the proxy: a proxy that has not seen
        the key answers 401, and the endpoint is reachable directly.
        """
        if not self._task_proxy:
            return
        proxy = shlex.quote(self._task_proxy)
        no_proxy = shlex.quote(self._no_proxy_value())
        content = "\n".join(
            [
                f"export http_proxy={proxy}",
                f"export https_proxy={proxy}",
                f"export HTTP_PROXY={proxy}",
                f"export HTTPS_PROXY={proxy}",
                f"export no_proxy={no_proxy}",
                f"export NO_PROXY={no_proxy}",
                "",
            ]
        )
        session.container.exec_run(
            [
                "sh",
                "-c",
                f"mkdir -p /etc/profile.d && echo {shlex.quote(content)} "
                "> /etc/profile.d/99-codape-proxy.sh",
            ]
        )

    def perform_task(
        self,
        instruction: str,
        session,
        logging_dir: Path | None = None,
    ) -> AgentResult:
        # Write the instruction as a file inside the container. Sending a
        # multi-line string through tmux would execute each line separately.
        self._install_task_proxy(session)
        rendered = self._render_instruction(instruction)
        handle = tempfile.NamedTemporaryFile(
            "w", suffix=".txt", delete=False, encoding="utf-8"
        )
        try:
            handle.write(rendered)
            handle.close()
            session.copy_to_container(
                Path(handle.name),
                container_dir=INSTALLED_DIR,
                container_filename="instruction.txt",
            )
            if self._binary_path is not None:
                session.copy_to_container(
                    self._binary_path,
                    container_dir="/tmp",
                    container_filename="codape-local-binary",
                )
        finally:
            Path(handle.name).unlink(missing_ok=True)

        result = super().perform_task(instruction, session, logging_dir)
        if result.failure_mode is not FailureMode.NONE:
            return result
        return self._with_runtime_details(result, session)

    def _render_instruction(self, instruction: str) -> str:
        if not self._extra_instructions:
            return instruction
        return f"{instruction}\n\n{self._extra_instructions}"

    def _run_agent_commands(self, instruction: str) -> list[TerminalCommand]:
        command = " ".join(
            shlex.quote(part)
            for part in [
                "timeout",
                "--signal=TERM",
                "--kill-after=30",
                str(int(self._timeout_sec)),
                CODAPE_BIN,
                "-p",
                "--dangerously-skip-permissions",
                "--no-session-persistence",
                "--no-telemetry",
                "--output-format",
                "stream-json",
                "--max-turns",
                str(self._max_turns),
                *self._extra_args,
            ]
        )
        # The harness appends "; tmux wait -S done" to blocking commands, so
        # this line must return to the shell. Do not call exit here.
        #
        # The tail is for a human watching the pane; it is not the record. The
        # full transcript is copied to /logs, which the harness mounts back to
        # the host, so a finished trial can be diagnosed after the container is
        # gone. `tail -n 20` was the only survivor of a run for exactly that
        # reason: the container and everything in /installed-agent's own
        # namespace went with it.
        command += (
            f" < {INSTRUCTION_PATH} > {self._stream_path} 2>&1"
            f'; rc=$?; printf "%s\\n" "$rc" > {self._exit_code_path}'
            f"; mkdir -p {LOGS_DIR} && cp {self._stream_path} {LOGS_STREAM_PATH} 2>/dev/null"
            f" && cp {self._exit_code_path} {LOGS_DIR}/codape-exit-code 2>/dev/null"
            f"; tail -n 20 {self._stream_path} 2>/dev/null; true"
        )
        return [
            TerminalCommand(
                command=command,
                min_timeout_sec=0.0,
                # The inner `timeout` bounds Codape itself. Leave the tmux
                # wait a little longer so the exit code is recorded rather
                # than racing the outer timeout.
                max_timeout_sec=self._timeout_sec + 120,
                block=True,
                append_enter=True,
            )
        ]

    def _with_runtime_details(self, result: AgentResult, session) -> AgentResult:
        exit_code = self._read_exit_code(session)
        summary = self._read_result_summary(session)
        usage = (summary or {}).get("usage")
        subtype = (summary or {}).get("subtype")
        failure_mode = result.failure_mode
        # Codape exits 0 even when the result frame reports an API/transport
        # error, so check the frame as well as the process status. A run that
        # did not finish for a reason other than the model stopping is a
        # failed run too: the container state is not an answer to the task.
        # `error_loop_detected` is included because the run was cut short
        # with work still in flight.
        #
        # `interrupted` is the frame Codape writes when a SIGTERM cut the
        # turn short. The inner `timeout` is what sends that signal, so it
        # means the same thing as the harness's own timer firing:
        # AGENT_TIMEOUT. Reporting it as a crash instead would hide the
        # reason the run stopped, and `is_error` on this frame must not
        # override the more specific mode.
        if subtype == "interrupted":
            failure_mode = FailureMode.AGENT_TIMEOUT
        elif (
            (exit_code is not None and exit_code != 0)
            or (summary or {}).get("is_error")
            # `error_no_output` is a run that spent its whole ceiling
            # reasoning and never wrote anything; `error_max_output_tokens`
            # is an answer that was cut short. Neither finished the task.
            or subtype
            in (
                "error_max_output_tokens",
                "error_no_output",
                "error_loop_detected",
            )
        ):
            failure_mode = FailureMode.UNKNOWN_AGENT_ERROR
        return AgentResult(
            total_input_tokens=(usage or {}).get("input_tokens") or 0,
            total_output_tokens=(usage or {}).get("output_tokens") or 0,
            failure_mode=failure_mode,
            timestamped_markers=result.timestamped_markers,
        )

    def _read_exit_code(self, session) -> int | None:
        try:
            exec_result = session.container.exec_run(["cat", self._exit_code_path])
        except Exception:
            return None
        if exec_result.exit_code != 0:
            return None
        try:
            return int(exec_result.output.decode("utf-8", errors="replace").strip())
        except ValueError:
            return None

    def _read_result_summary(self, session) -> dict | None:
        # The result frame is the last line; read a bounded tail rather than a
        # potentially large transcript.
        try:
            exec_result = session.container.exec_run(
                ["sh", "-c", f"tail -c 262144 {self._stream_path} 2>/dev/null"]
            )
        except Exception:
            return None
        if exec_result.exit_code != 0:
            return None
        return parse_result_summary(exec_result.output.decode("utf-8", errors="replace"))


def parse_result_summary(text: str) -> dict | None:
    """Return error status and usage from the last Codape result frame.

    Codape 0.2.0 does not populate usage in headless stream-json today, so
    ``usage`` is normally ``None``. ``is_error`` is meaningful now: the CLI
    exits 0 after an API failure but sets it on the result frame.
    """
    last_result = None
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            frame = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(frame, dict) and frame.get("type") == "result":
            last_result = frame
    if last_result is None:
        return None
    usage = last_result.get("usage")
    parsed_usage = None
    if isinstance(usage, dict):
        parsed_usage = {
            "input_tokens": int(usage.get("input_tokens") or 0),
            "output_tokens": int(usage.get("output_tokens") or 0),
        }
    return {
        "is_error": bool(last_result.get("is_error")),
        "subtype": last_result.get("subtype"),
        "usage": parsed_usage,
    }
