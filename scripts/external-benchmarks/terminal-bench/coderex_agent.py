"""Terminal-Bench adapter that runs the CodeRex CLI inside the task container.

Terminal-Bench installs an agent into the task's Docker container and then
drives a tmux session in that container. This adapter follows the same shape as
the built-in installed agents:

* ``coderex-setup.sh.j2`` downloads or copies a pinned CodeRex release.
* ``_run_agent_commands`` runs ``coderex -p`` headlessly in that tmux session.
* ``perform_task`` copies the task instruction into the container as a file so
  that newlines in the instruction cannot be interpreted by tmux as early
  Enter presses.

The harness evaluates container state after the agent exits; there is no
"answer" string to return. The adapter also records CodeRex's exit status and,
when the CLI emits it, stream-json token usage in the ``AgentResult``.

Usage with ``tb`` (see run_terminal_bench.py for the wrapper):

    tb run \
      --dataset terminal-bench-core==0.1.1 \
      --agent-import-path coderex_agent:CodeRexAgent \
      --agent-kwarg version=v0.1.3 \
      --model claude-sonnet-4-5-20250929
"""

from __future__ import annotations

import json
import os
import shlex
import tempfile
from pathlib import Path

from terminal_bench.agents.base_agent import AgentResult
from terminal_bench.agents.failure_mode import FailureMode
from terminal_bench.agents.installed_agents.abstract_installed_agent import (
    AbstractInstalledAgent,
)
from terminal_bench.terminal.models import TerminalCommand

# Fixed paths inside the task container. /installed-agent is where
# AbstractInstalledAgent puts the setup script and is mounted back to the host.
CODEREX_BIN = "/usr/local/bin/coderex"
INSTALLED_DIR = "/installed-agent"
INSTRUCTION_PATH = f"{INSTALLED_DIR}/instruction.txt"
STREAM_PATH = f"{INSTALLED_DIR}/coderex-stream.jsonl"
EXIT_CODE_PATH = f"{INSTALLED_DIR}/coderex-exit-code"
CONFIG_DIR = "/tmp/coderex-config"
LOCAL_BINARY_PATH = "/tmp/coderex-local-binary"

DEFAULT_VERSION = "v0.1.3"
DEFAULT_MAX_TURNS = 200
DEFAULT_TIMEOUT_SEC = 1800

# Host variables forwarded into the container. CodeRex itself applies this
# precedence; copying the whole set keeps the adapter out of the way when a
# user configures only one provider. Values come from the host environment and
# are never printed by this module.
PASSTHROUGH_ENV = (
    "CODEREX_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "CODEREX_BASE_URL",
    "ANTHROPIC_BASE_URL",
    "OPENAI_BASE_URL",
    "CODEREX_MODEL",
    "ANTHROPIC_MODEL",
    "OPENAI_MODEL",
    "CODEREX_CUSTOM_HEADERS",
)


class CodeRexAgent(AbstractInstalledAgent):
    """Installed agent that drives the CodeRex headless CLI."""

    def __init__(
        self,
        model_name: str | None = None,
        version: str = DEFAULT_VERSION,
        binary_path: str | None = None,
        max_turns: int | str = DEFAULT_MAX_TURNS,
        timeout_sec: float | str = DEFAULT_TIMEOUT_SEC,
        extra_args: str | list[str] | None = None,
        **kwargs,
    ):
        super().__init__(**kwargs)
        self._model_name = str(model_name).removeprefix("anthropic/") if model_name else None
        self._version = str(version)
        self._binary_path = Path(str(binary_path)).expanduser() if binary_path else None
        self._max_turns = int(max_turns)
        self._timeout_sec = float(timeout_sec)
        if isinstance(extra_args, str):
            self._extra_args = shlex.split(extra_args)
        elif extra_args is None:
            self._extra_args = []
        else:
            self._extra_args = [str(arg) for arg in extra_args]

        if self._binary_path is not None and not self._binary_path.exists():
            raise FileNotFoundError(f"CodeRex binary does not exist: {self._binary_path}")

        # Moved into the host-mounted /agent-logs directory when the harness
        # supplies a logging directory, so the transcript survives the
        # container. Falls back to the plain container path for manual use.
        self._stream_path = STREAM_PATH
        self._exit_code_path = EXIT_CODE_PATH

    @staticmethod
    def name() -> str:
        return "coderex"

    @property
    def _env(self) -> dict[str, str]:
        env = {
            name: value
            for name in PASSTHROUGH_ENV
            if (value := os.environ.get(name))
        }
        if self._model_name:
            env["CODEREX_MODEL"] = self._model_name
        # Keep CodeRex's state out of the task's working tree and skip the
        # first-run telemetry consent prompt. CODEREX_LOG=off keeps the tmux
        # pane readable.
        env["CLAUDE_CONFIG_DIR"] = CONFIG_DIR
        env["CODEREX_LOG"] = "off"
        env["NO_COLOR"] = "1"
        return env

    def _create_env_setup_file(self) -> str:
        # Same purpose as the base implementation, but shlex-quoted so a value
        # containing a single quote or shell metacharacter cannot break the
        # setup line or leak into the terminal.
        return "\n".join(
            f"export {key}={shlex.quote(value)}" for key, value in self._env.items()
        )

    def _get_template_variables(self) -> dict[str, object]:
        return {
            "version": self._version,
            "target": "x86_64-unknown-linux-musl",
            "use_local_binary": self._binary_path is not None,
        }

    @property
    def _install_agent_script_path(self) -> Path:
        return self._get_templated_script_path("coderex-setup.sh.j2")

    def perform_task(
        self,
        instruction: str,
        session,
        logging_dir: Path | None = None,
    ) -> AgentResult:
        if logging_dir is not None:
            self._stream_path = f"{self.CONTAINER_AGENT_LOGS_PATH}/coderex-stream.jsonl"
            self._exit_code_path = f"{self.CONTAINER_AGENT_LOGS_PATH}/coderex-exit-code"

        # Write the instruction as a file inside the container. Sending a
        # multi-line string through tmux would execute each line separately.
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
                    container_filename="coderex-local-binary",
                )
        finally:
            Path(handle.name).unlink(missing_ok=True)

        result = super().perform_task(instruction, session, logging_dir)
        if result.failure_mode is not FailureMode.NONE:
            return result
        return self._with_runtime_details(result, session)

    def _run_agent_commands(self, instruction: str) -> list[TerminalCommand]:
        command = " ".join(
            shlex.quote(part)
            for part in [
                "timeout",
                "--signal=TERM",
                "--kill-after=30",
                str(int(self._timeout_sec)),
                CODEREX_BIN,
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
        command += (
            f" < {INSTRUCTION_PATH} > {self._stream_path} 2>&1"
            f'; rc=$?; printf "%s\\n" "$rc" > {self._exit_code_path}'
            f"; tail -n 20 {self._stream_path} 2>/dev/null; true"
        )
        return [
            TerminalCommand(
                command=command,
                min_timeout_sec=0.0,
                # The inner `timeout` bounds CodeRex itself. Leave the tmux
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
        failure_mode = result.failure_mode
        # CodeRex exits 0 even when the result frame reports an API/transport
        # error, so check the frame as well as the process status.
        if (exit_code is not None and exit_code != 0) or (summary or {}).get("is_error"):
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
    """Return error status and usage from the last CodeRex result frame.

    CodeRex 0.1.3 does not populate usage in headless stream-json today, so
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
        "usage": parsed_usage,
    }
