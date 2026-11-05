"""Terminal-Bench adapter that drives Claude Code against the same model as CodeRex.

This exists to answer one question: when a task fails under CodeRex, is the
model the limit, or is CodeRex's scaffold the limit? Claude Code is pointed at
the same provider endpoint and the same model, so a task that passes here and
fails under CodeRex points at the scaffold; one that fails both ways points at
the model (or the task).

The built-in ``terminal_bench`` Claude Code agent is reused for installation and
the tool loop; the only additions are the provider environment (the built-in
one forwards ``ANTHROPIC_API_KEY`` but not ``ANTHROPIC_BASE_URL``) and a
``--max-turns`` ceiling.
"""

from __future__ import annotations

import os
import shlex
import tempfile
from pathlib import Path

from terminal_bench.agents.installed_agents.claude_code import (
    claude_code_agent as _claude_code_module,
)
from terminal_bench.agents.installed_agents.claude_code.claude_code_agent import (
    ClaudeCodeAgent,
)
from terminal_bench.terminal.models import TerminalCommand
from terminal_bench.utils.template_utils import render_setup_script


class ClaudeAgent(ClaudeCodeAgent):
    """Claude Code installed into the task container, pointed at our endpoint."""

    def __init__(
        self,
        model_name: str | None = None,
        version: str = "latest",
        max_turns: int | str | None = None,
        timeout_sec: float | str | None = None,
        **kwargs,
    ):
        super().__init__(model_name=model_name, version=version, **kwargs)
        self._max_turns = int(max_turns) if max_turns else None
        # Accepted for symmetry with the CodeRex adapter; the harness enforces
        # the wall clock with --global-agent-timeout-sec.
        self._timeout_sec = float(timeout_sec) if timeout_sec else None

    @property
    def _env(self) -> dict[str, str]:
        env = dict(super()._env)
        base_url = os.environ.get("ANTHROPIC_BASE_URL") or os.environ.get(
            "CODEREX_BASE_URL"
        )
        if base_url:
            env["ANTHROPIC_BASE_URL"] = base_url
        # Claude Code reads the bearer token from ANTHROPIC_AUTH_TOKEN; the
        # provider key is the same value the CodeRex runs used.
        token = os.environ.get("ANTHROPIC_AUTH_TOKEN") or os.environ.get(
            "CODEREX_API_KEY"
        )
        if token:
            env["ANTHROPIC_AUTH_TOKEN"] = token
        headers = os.environ.get("ANTHROPIC_CUSTOM_HEADERS") or os.environ.get(
            "CODEREX_CUSTOM_HEADERS"
        )
        if headers:
            env["ANTHROPIC_CUSTOM_HEADERS"] = headers
        # Keep Claude Code's state out of the task's working tree, and tell it
        # this is a disposable container (it refuses some flags as root).
        env["CLAUDE_CONFIG_DIR"] = "/tmp/claude-config"
        env["IS_SANDBOX"] = "1"
        return env

    @property
    def _install_agent_script_path(self) -> Path:
        # AbstractInstalledAgent looks for the template next to the *subclass*
        # file, so it would search this script's directory. Point it at the
        # built-in template instead of keeping a copy here that can drift.
        template_path = (
            Path(_claude_code_module.__file__).parent / "claude-code-setup.sh.j2"
        )
        script = render_setup_script(template_path, self._get_template_variables())
        handle = tempfile.NamedTemporaryFile(mode="w", suffix=".sh", delete=False)
        handle.write(script)
        handle.close()
        os.chmod(handle.name, 0o755)
        return Path(handle.name)

    def _run_agent_commands(self, instruction: str) -> list[TerminalCommand]:
        tools = [*self.ALLOWED_TOOLS, "WebSearch"]
        command = (
            "source $HOME/.nvm/nvm.sh >/dev/null 2>&1 || true; "
            "claude --verbose --output-format stream-json "
            f"-p {shlex.quote(instruction)} --allowedTools {' '.join(tools)}"
        )
        if self._max_turns:
            command += f" --max-turns {self._max_turns}"
        return [
            TerminalCommand(
                command=command,
                min_timeout_sec=0.0,
                max_timeout_sec=float("inf"),
                block=True,
                append_enter=True,
            ),
        ]
