"""Harbor adapter that runs the Codape CLI inside the task environment.

This is the Harbor counterpart of ``codape_agent.py``. That file targets
``terminal-bench==0.2.18``, whose harness is frozen; Harbor
(`harbor==0.27.0`) is the harness Terminal-Bench 4.1 (``v4.1.0``) is published
against, so a current leaderboard row has to come from here. The two adapters
share no base class and are kept side by side: the old one still works against
the pinned dataset, and deleting it would strand the runs recorded in
``docs/terminal-bench.md``.

What differs from the 0.2.18 adapter, and why:

* ``BaseInstalledAgent`` is **async**. ``install`` and ``run`` are coroutines
  that drive the environment through ``exec_as_root``/``exec_as_agent`` rather
  than tmux sessions, so there is no ``TerminalCommand`` and no "append
  `; tmux wait`" dance.
* Per-run state lives on ``configure()``, not in a Jinja template the harness
  sources. ``install()`` places the binary; ``configure()`` writes the settings
  and the task proxy, because those can change between runs of one image.
* Trajectories are **ATIF** (``trajectory.json``). Harbor renders them in its
  viewer, so the conversion is a real deliverable rather than telemetry. It is
  a pure function over the transcript so it can be checked without Docker.

Usage:

    harbor run -d terminal-bench/terminal-bench@4.1.0 \\
      --agent codape_harbor_agent:CodapeHarborAgent \\
      --model anthropic/claude-sonnet-4-5 \\
      -k 4

``--agent`` takes a `module:Class` import path (not a file path) — the module
must be importable, e.g. by running from this directory.
"""

from __future__ import annotations

import json
import os
import shlex
from pathlib import Path

from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.agents.agent_auth_resolution import ModelConnectionSpec, ProviderAccess
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.model_api_format import ModelApiFormat
from harbor.models.trajectories import (
    Agent,
    FinalMetrics,
    Observation,
    ObservationResult,
    Step,
    ToolCall,
    Trajectory,
)
from harbor.trial.errors import AgentTimeoutError

CODAPE_BIN = "/usr/local/bin/codape"
CONFIG_DIR = "/tmp/codape-config"
# The task's MCP servers, in Codape's own `.mcp.json` shape, and the price
# table its cost tracker reads at startup. Both live in the config home rather
# than the working tree: a `.mcp.json` left in the task directory would show up
# in `git status` and in every artifact the trial collects.
MCP_CONFIG_PATH = f"{CONFIG_DIR}/mcp.json"
PRICING_PATH = f"{CONFIG_DIR}/pricing.json"
INSTALLED_DIR = "/installed-agent"
INSTRUCTION_PATH = f"{INSTALLED_DIR}/instruction.txt"
STREAM_PATH = f"{INSTALLED_DIR}/codape-stream.jsonl"
EXIT_CODE_PATH = f"{INSTALLED_DIR}/codape-exit-code"
LOCAL_BINARY_PATH = "/tmp/codape-local-binary"

DEFAULT_VERSION = "v0.2.2"
DEFAULT_MAX_TURNS = 300
RELEASE_TARGET = "x86_64-unknown-linux-musl"

# How long Codape itself may run, before the adapter's own `timeout` signals it.
#
# `None` — the default — means the adapter imposes no wall clock of its own and
# Harbor's `[agent] timeout_sec` is what ends a run that has gone too long. That
# is the only value that is right for every configuration: Harbor bounds
# `run()` with the *task's* budget (`_compute_agent_timeout_sec`, which also
# applies `--timeout-multiplier`) and raises `AgentTimeoutError`, but it passes
# that number only to the oracle agent (`Trial._init_agent`), so an installed
# agent cannot read it. Any fixed number here is therefore either longer than
# the budget, in which case it never fires, or shorter, in which case it ends a
# run Harbor was still willing to pay for and reports the unfinished work as a
# wrong answer.
#
# That is exactly what the previous hardcoded 1740 s did: all 66 tasks in the
# released `terminal-bench/terminal-bench@4.1.0` declare a 28800 s agent budget
# (63 came down in a `harbor datasets download`; the other three were read from
# the tag's own `task.toml`), so the adapter stopped 7.5 hours early on every
# one. `claude_code.py`, the reference agent for these leaderboards, likewise
# passes no timeout and lets the harness decide.
#
# Set it (`--ak timeout_sec=28000`) only to trade a little wall clock for a
# graceful ending: the inner `timeout` sends SIGTERM, Codape answers with an
# `interrupted` result frame carrying what the provider billed, and the trial is
# then reported as a timeout (see `_timed_out`) instead of a completed answer.
DEFAULT_TIMEOUT_SEC = None

# Slack between the inner `timeout` firing and the exec transport giving up, so
# the exit status reaches EXIT_CODE_PATH instead of being cut off by the
# transport. Only meaningful when `timeout_sec` is set.
EXIT_GRACE_SEC = 120

# `localhost` and loopback stay out of the proxy: a task that starts its own
# service and polls it over loopback must not send that request through a
# proxy, and tasks do this constantly.
NO_PROXY = "localhost,127.0.0.1,::1,host.docker.internal"

# Host variables forwarded into the environment. Codape applies this precedence
# itself; copying the whole set keeps the adapter out of the way when a user
# configures only one provider. Values are never printed by this module.
PASSTHROUGH_ENV = (
    "CODAPE_API_KEY",
    "CODAPE_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "CODAPE_BASE_URL",
    "ANTHROPIC_BASE_URL",
    "OPENAI_BASE_URL",
    "CODAPE_MODEL",
    "ANTHROPIC_MODEL",
    "OPENAI_MODEL",
    "CODAPE_CUSTOM_HEADERS",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CODAPE_CONTEXT_WINDOW",
    "CODAPE_MAX_TOKENS",
)


class CodapeHarborAgent(BaseInstalledAgent):
    """Installed agent that drives the Codape headless CLI under Harbor."""

    # `atif=True` is a promise that `self.logs_dir / "trajectory.json"` holds a
    # valid ATIF document. `build_trajectory` below is what keeps that promise;
    # if it ever stops being written, this flag has to come off rather than
    # stay a claim the code does not honour.
    #
    # `mcp_servers=True` is the same kind of promise, and it was missing for a
    # measurable reason: Harbor refuses to set up a trial whose task declares
    # MCP servers unless the agent claims them (`trial.py`,
    # `_validate_agent_capabilities`), so `medical-claims-processing` — whose
    # `task.toml` declares a `playwright` SSE server — died with "Agent 'codape'
    # does not support MCP servers" before any agent code ran. Codape has a full
    # MCP client (`crates/mcp/`) and `--mcp-config`; `_write_mcp_config` below is
    # what turns `self.mcp_servers` into the file it reads. The declaration is
    # only honest while that function exists.
    #
    # `mcp_http_headers=True` because the headers are written into that same
    # file. A header the task secrets through `${VAR}` is resolved here and
    # placed in the file, not in the argv — the config file is mode 0600 and the
    # argv is visible to `ps`.
    #
    # `skills=True` was the same missing promise, one task wide:
    # `cumulative-layout-shift` declares `skills_dir = "/app/barber-shop-site/
    # .agents/skills"` under both `[environment]` and `[verifier.environment]`,
    # and the trial died in `_validate_agent_capabilities` before starting. The
    # package source has no `.agents/` directory — but the built image does:
    # `agent-browser/SKILL.md`, 3263 bytes, and Codape parses it
    # (`SkillLoader::load_from_dir`, directory form). `_register_skills_command`
    # is what puts that directory on a discovery root.
    #
    # The honest limit: Codape reads `allowed_tools` (underscore) and this skill
    # writes `allowed-tools` (hyphen, the Claude Code spelling), so the tool
    # hint is dropped. Unknown frontmatter keys are skipped silently, the skill
    # still loads and is still announced, and Codape never enforced per-skill
    # tool lists anyway (`allowed_tools` is carried, not applied — the enforced
    # one is `allowed_tools_override`), so the cost is a hint, not a difference
    # in permission.
    capabilities = AgentCapabilities(
        atif=True,
        base_url=True,
        mcp_servers=True,
        mcp_http_headers=True,
        skills=True,
    )

    # `base_url=True` is only honest with a spec behind it: Harbor raises
    # "Agent 'codape' does not support inference connection overrides" for
    # `--base-url` whenever `MODEL_CONNECTION` is None (base.py, `__init__`),
    # and `_network_plan` reads `model_connection.inference_base_url` to
    # allowlist the inference host — with no spec nothing is inferred and an
    # allowlist network policy silently blocks the model API. Declaring the
    # spec is what makes both work.
    #
    # Destinations are Codape's own spellings, which its resolver checks before
    # the ANTHROPIC_ ones, so a Harbor-selected key or endpoint outranks
    # whatever the host happened to export.
    MODEL_CONNECTION = ModelConnectionSpec(
        default_provider="anthropic",
        agent_provider_access=ProviderAccess(
            api_key_candidate_names=[
                "CODAPE_API_KEY",
                "CODAPE_AUTH_TOKEN",
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
            ],
            base_url_candidate_names=["CODAPE_BASE_URL", "ANTHROPIC_BASE_URL"],
        ),
        api_key_destination_names={"default": "CODAPE_API_KEY"},
        base_url_destination_names={"default": "CODAPE_BASE_URL"},
        supported_api_formats=[ModelApiFormat.ANTHROPIC_MESSAGES],
        default_api_format_by_provider={"default": ModelApiFormat.ANTHROPIC_MESSAGES},
    )

    def __init__(
        self,
        binary_path: str | None = None,
        version: str = DEFAULT_VERSION,
        max_turns: int = DEFAULT_MAX_TURNS,
        timeout_sec: float | str | None = DEFAULT_TIMEOUT_SEC,
        task_proxy: str | None = None,
        extra_args: str | None = None,
        extra_instructions: str | None = None,
        pricing_path: str | None = None,
        **kwargs,
    ):
        super().__init__(**kwargs)
        self._local_binary = (
            Path(str(binary_path)).expanduser() if binary_path else None
        )
        if self._local_binary is not None and not self._local_binary.exists():
            raise FileNotFoundError(
                f"Codape binary does not exist: {self._local_binary}"
            )
        self._version = str(version)
        self._max_turns = int(max_turns)
        # `--ak timeout_sec=...` arrives as a string; the CLI parses kwargs as
        # key=value pairs. Empty means "no adapter-imposed ceiling".
        self._timeout_sec = (
            float(timeout_sec) if timeout_sec not in (None, "") else None
        )
        self._task_proxy = str(task_proxy) if task_proxy else None
        self._extra_args = shlex.split(extra_args) if extra_args else []
        self._extra_instructions = (
            str(extra_instructions).strip() if extra_instructions else None
        )
        # Host-side JSON price table, uploaded into the config home as
        # `pricing.json`. Read and validated here rather than at `configure`
        # time: a malformed file should fail before the trial spends a
        # container, and a bad table would otherwise be invisible until the
        # result frame turned out to carry no `total_cost_usd` at all.
        self._pricing = _read_pricing(pricing_path)

    @staticmethod
    def name() -> str:
        return "codape"

    # -- setup ---------------------------------------------------------

    async def install(self, environment: BaseEnvironment) -> None:
        """Put the Codape binary in place.

        Only what could be baked into an image belongs here; the settings file
        and proxy fragment are per-run and live in ``configure``.
        """
        if self._local_binary is not None:
            await environment.upload_file(
                str(self._local_binary), LOCAL_BINARY_PATH
            )
            await self.exec_as_root(
                environment,
                command=f"install -m 0755 {LOCAL_BINARY_PATH} {CODAPE_BIN}",
            )
        else:
            # A task image is not required to carry a downloader. The first one
            # this adapter was run against — `cargo-flight-dispatch`, whose
            # base is `python:3.13-slim-bookworm` — has no `curl` at all, so
            # the release step exited 127 before any task work started. Harbor
            # knows the package name per distro and installs it through the
            # image's own package manager, which is why the reference
            # implementation (`claude_code.py`) calls this before it curls
            # anything.
            await self.ensure_system_dependencies(
                environment, ("curl", "ca_certificates", "xz")
            )
            # A release download has to be verified: an unverified tarball run
            # as root in the task container is a supply-chain hole. The
            # checksum is published beside the archive.
            #
            # `--retry-all-errors` is not decoration: the first run of this
            # step died with `curl: (18) HTTP/2 stream 1 was not closed cleanly`
            # partway through a 20 MB archive. Without a retry that single
            # truncated stream fails the whole trial, which is indistinguishable
            # from a broken adapter or a weak model in the results.
            await self.exec_as_root(
                environment,
                command=(
                    "set -e; "
                    'TMP="$(mktemp -d)"; '
                    f'BASE="https://github.com/lightconsen/codape/releases/download/{self._version}"; '
                    f'ARCHIVE="codape-{RELEASE_TARGET}.tar.gz"; '
                    'CURL="curl -fsSL --retry 5 --retry-all-errors --retry-delay 2"; '
                    '$CURL "$BASE/$ARCHIVE" -o "$TMP/$ARCHIVE"; '
                    '$CURL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS"; '
                    'EXPECTED="$(awk -v n="$ARCHIVE" \'$2 == n { print $1; exit }\' "$TMP/SHA256SUMS")"; '
                    '[ -n "$EXPECTED" ] || { echo "SHA256SUMS has no entry for $ARCHIVE" >&2; exit 1; }; '
                    'ACTUAL="$(sha256sum "$TMP/$ARCHIVE" | awk \'{ print $1 }\')"; '
                    '[ "$ACTUAL" = "$EXPECTED" ] || { echo "checksum mismatch" >&2; exit 1; }; '
                    'tar -xzf "$TMP/$ARCHIVE" -C "$TMP"; '
                    f'install -m 0755 "$TMP/codape-{RELEASE_TARGET}" {CODAPE_BIN}; '
                    'rm -rf "$TMP"'
                ),
            )

    async def configure(self, environment: BaseEnvironment) -> None:
        """Write per-run state: settings, MCP servers, prices, and the proxy.

        All of it is per-run rather than part of ``install``: the settings file
        names this run's telemetry choice, ``mcp.json`` is this *task's* server
        list, and the price table is this run's model. Nothing here can be
        baked into an image.
        """
        await self.exec_as_root(
            environment,
            command=(
                f"mkdir -p {CONFIG_DIR} {INSTALLED_DIR} && "
                f"printf '%s\\n' '{{\"config_version\": 4, \"telemetry_enabled\": false}}' "
                f"> {CONFIG_DIR}/settings.json"
            ),
        )
        # The settings file is written 0644, so any user can read it; the two
        # files below carry a resolved credential and are 0600, which means
        # they need the owner the agent will actually run as.
        agent_user = getattr(environment, "default_user", None)
        mcp_command = self._register_mcp_servers_command(agent_user)
        if mcp_command:
            await self.exec_as_root(environment, command=mcp_command)
        pricing_command = self._register_pricing_command(agent_user)
        if pricing_command:
            await self.exec_as_root(environment, command=pricing_command)
        skills_command = self._register_skills_command()
        if skills_command:
            await self.exec_as_root(environment, command=skills_command)
        if self._task_proxy:
            # The tests run in their own shell, and several task scripts install
            # their tooling there (the `uv` bootstrap is the common one). A
            # profile fragment covers the test phase as well as the agent's.
            lines = [
                f"export http_proxy={self._task_proxy}",
                f"export https_proxy={self._task_proxy}",
                f"export HTTP_PROXY={self._task_proxy}",
                f"export HTTPS_PROXY={self._task_proxy}",
                f"export no_proxy={self._no_proxy_value()}",
                f"export NO_PROXY={self._no_proxy_value()}",
            ]
            await self.exec_as_root(
                environment,
                command=(
                    "mkdir -p /etc/profile.d && "
                    f"printf '%s\\n' {shlex.quote(chr(10).join(lines))} "
                    "> /etc/profile.d/99-codape-proxy.sh"
                ),
            )

    def _register_mcp_servers_command(
        self, agent_user: str | int | None = None
    ) -> str | None:
        """Write the task's MCP servers where Codape will read them.

        Harbor hands the agent ``self.mcp_servers`` — already merged from the
        task's ``[environment.mcp_servers]`` and any agent-level ones — in its
        own model, whose field names are nearly Codape's. The mapping that is
        *not* identity:

        * ``transport``: Harbor spells streamable HTTP ``streamable-http``;
          Codape's ``McpTransport`` accepts that as an alias for ``http``, so it
          passes through unchanged. ``sse`` and ``stdio`` are shared spellings.
        * ``url`` / ``command`` / ``args``: same names, same meaning.
        * ``headers``: Harbor resolves ``${VAR}`` templates at construction and
          exposes the results as ``mcp_header_env`` — a private exec env whose
          variable names go into the config, never the values. Codape expands no
          templates in a header (``crates/mcp/src/http.rs`` sends the map as
          configured), so the resolved value has to be written here. That is
          why the file is created 0600, and why ``agent_user`` matters: this
          adapter writes as root, and a task that runs its agent as another
          user (`[agent] user`) would otherwise get a config only root can read.

        The path is given to Codape explicitly as ``--mcp-config`` in
        ``_command()`` rather than relied on as ``<cwd>/.mcp.json``: the agent's
        working directory is the task's, and a config file left there would be
        part of the working tree a verifier reads and an artifact captures.

        Returns ``None`` when the task declares no servers, so ``configure``
        writes no empty file — Codape skips an empty config with a warning, and
        a stray file would be noise in every run of the other 59 of the 60
        cached 4.1 task packages (47 say nothing about MCP; 12 carry
        ``mcp_servers = []``, which is Harbor's spelling for none).
        """
        if not self.mcp_servers:
            return None
        header_refs = self.mcp_header_env.refs
        header_env = self.mcp_header_env.env
        servers: dict[str, dict[str, object]] = {}
        for server in self.mcp_servers:
            if server.transport == "stdio":
                servers[server.name] = {
                    "type": "stdio",
                    "command": server.command,
                    "args": list(server.args),
                }
            else:
                entry: dict[str, object] = {
                    "type": server.transport,
                    "url": server.url,
                }
                resolved = {
                    name: header_env[variable]
                    for name, variable in header_refs.get(server.name, {}).items()
                    if variable in header_env
                }
                if resolved:
                    entry["headers"] = resolved
                servers[server.name] = entry
        payload = json.dumps({"mcpServers": servers}, indent=2)
        # `umask 077` rather than a chmod afterwards, so the file is never
        # world-readable even for the moment between creation and the chmod.
        command = (
            f"umask 077 && printf '%s\\n' {shlex.quote(payload)} "
            f"> {MCP_CONFIG_PATH} && chmod 600 {MCP_CONFIG_PATH}"
        )
        if agent_user is not None and str(agent_user) != "0":
            owner = shlex.quote(str(agent_user))
            command += f" && chown {owner} {MCP_CONFIG_PATH}"
        return command

    def _register_pricing_command(
        self, agent_user: str | int | None = None
    ) -> str | None:
        """Write the host's price table into the config home, if one was given.

        Codape refuses to emit a dollar total for a model it has no price for
        (`known_total_cost` returns ``None`` once `unpriced_usage` is set), and
        its built-in table holds Claude models only. That refusal is right; what
        made the first comparison arm incomparable on cost was that the adapter
        never supplied the one input that resolves it. ``CostTracker::new``
        already loads ``pricing.json`` from the config home — which is
        ``CLAUDE_CONFIG_DIR``, i.e. ``CONFIG_DIR`` — and prefers it to the
        built-in table.

        The file is a plain map of model id to the four per-million rates;
        ``_read_pricing`` has already validated its shape. Returns ``None``
        without one, leaving Codape's own refusal in place rather than inventing
        a price. ``agent_user`` has the same meaning as in
        ``_register_mcp_servers_command``: the file is 0600 and this adapter
        writes as root.
        """
        if not self._pricing:
            return None
        payload = json.dumps(self._pricing, indent=2, sort_keys=True)
        command = (
            f"umask 077 && printf '%s\\n' {shlex.quote(payload)} > {PRICING_PATH}"
        )
        if agent_user is not None and str(agent_user) != "0":
            owner = shlex.quote(str(agent_user))
            command += f" && chown {owner} {PRICING_PATH}"
        return command

    def _register_skills_command(self) -> str | None:
        """Point Codape's user skill root at the task's skills directory.

        Codape's discovery roots are ``<config home>/skills``,
        ``<repo>/.codape/skills``, plugins and the bundled tree
        (``crates/plugins/src/skills.rs``, ``SkillLoader::load_all``). Harbor's
        ``skills_dir`` is a fourth, unrelated location that the task names and
        Harbor passes to the agent verbatim — for
        ``cumulative-layout-shift`` it is
        ``/app/barber-shop-site/.agents/skills``, inside the task image.

        A symlink joins them. Copying would work for the single-file form of a
        skill but silently drop the sibling files of the directory form
        (``<name>/SKILL.md`` plus its templates and scripts) — and the one skill
        actually shipped in a 4.1 task is the directory form.

        The link lives in the config home rather than the working tree, so it
        cannot appear in an artifact the verifier reads. It is created only when
        the path exists: Harbor raises on a *declared* ``skills_dir`` whether or
        not the directory is really there, so an absent path means the task
        names a location it never populated, and an empty link would be a lie
        about what the agent was given.
        """
        if not self.skills_dir:
            return None
        target = shlex.quote(str(self.skills_dir))
        link = f"{CONFIG_DIR}/skills"
        return (
            f"if [ -e {target} ]; then "
            f"rm -rf {link} && ln -s {target} {link}; fi"
        )

    def _no_proxy_value(self) -> str:
        # NO_PROXY is one comma-separated string; split it so deduplication
        # below compares hosts and not the whole literal.
        hosts = [host for host in NO_PROXY.split(",") if host]
        endpoints = [
            os.environ.get(name)
            for name in ("ANTHROPIC_BASE_URL", "CODAPE_BASE_URL", "OPENAI_BASE_URL")
        ]
        # A `--base-url` selected through Harbor never touches the host
        # environment. Treat it like the ones that do: this function's existing
        # decision is that the inference endpoint is reached directly rather
        # than through the task proxy, and leaving the Harbor-selected URL out
        # would make the same endpoint proxied or not depending on how it was
        # configured.
        connection = self.model_connection
        endpoints.append(
            connection.user_supplied_base_url or connection.candidate_base_url
        )
        for value in endpoints:
            if not value:
                continue
            host = value.split("://", 1)[-1].split("/", 1)[0].split(":", 1)[0]
            if host:
                hosts.append(host)
        return ",".join(dict.fromkeys(hosts))

    def _resolved_model_name(self) -> str | None:
        """The model id Codape should actually ask the API for.

        Harbor hands ``--model anthropic/claude-sonnet-4-5`` — a LiteLLM
        spelling whose provider prefix Harbor itself consumes. Codape takes a
        bare model id and forwards it verbatim (``ModelId::from_alias`` only
        rewrites the short aliases), so passing the prefixed form through
        would send the API a name it does not know.

        Harbor's own parse is preferred when it produced one; the fallback
        covers the stubbed-CI path and a model it could not parse.
        """
        if not self.model_name:
            return None
        connection = self.model_connection
        if connection.provider in (None, "anthropic") and connection.model_id:
            return connection.model_id
        return strip_provider_prefix(self.model_name, connection.provider)

    def _env(self) -> dict[str, str]:
        env = {
            name: value
            for name in PASSTHROUGH_ENV
            if (value := os.environ.get(name))
        }
        # Harbor resolves `--base-url` / `--api-key-name` / the agent overlay
        # into `model_connection.env`, keyed by the destinations declared in
        # MODEL_CONNECTION. Nothing applies that dict for a custom agent —
        # claude_code's `_resolve_auth_env` is the per-agent equivalent — so a
        # run that selected a credential through Harbor would otherwise launch
        # Codape with whatever the host happened to export. Applied after the
        # passthrough so the deliberate selection wins.
        env.update(self.model_connection.env)
        model = self._resolved_model_name()
        if model:
            env["CODAPE_MODEL"] = model
        # Either spelling configures the same gateway headers; Codape reads
        # CODAPE_CUSTOM_HEADERS, so fill it from whichever is set.
        headers = env.get("CODAPE_CUSTOM_HEADERS") or env.get("ANTHROPIC_CUSTOM_HEADERS")
        if headers:
            env["CODAPE_CUSTOM_HEADERS"] = headers
        # Keep state out of the task's working tree and skip the first-run
        # telemetry consent prompt.
        env["CLAUDE_CONFIG_DIR"] = CONFIG_DIR
        env["CODAPE_LOG"] = "off"
        env["NO_COLOR"] = "1"
        # The container is the isolation boundary. Codape's shell sandbox
        # denies network and the privilege-drop syscalls apt needs, so package
        # installation would be impossible with it on.
        env["CODAPE_SANDBOX"] = "off"
        if self._task_proxy:
            env["http_proxy"] = self._task_proxy
            env["https_proxy"] = self._task_proxy
            env["no_proxy"] = self._no_proxy_value()
        return env

    # -- run -----------------------------------------------------------

    def _render_instruction(self, instruction: str) -> str:
        if not self._extra_instructions:
            return instruction
        return f"{instruction}\n\n{self._extra_instructions}"

    def _command(self) -> str:
        prefix = []
        if self._timeout_sec is not None:
            prefix = ["timeout", "--signal=TERM", "--kill-after=30",
                      str(int(self._timeout_sec))]
        parts = [
            *prefix,
            CODAPE_BIN,
            "-p",
            "--dangerously-skip-permissions",
            "--no-session-persistence",
            "--no-telemetry",
            "--output-format",
            "stream-json",
            "--max-turns",
            str(self._max_turns),
            # Without this a `tool_use` frame carries `{"summary": "Running a
            # command"}` and the ATIF trajectory the adapter writes names the
            # tool but not what it was told to do — a 300-call run whose
            # trajectory cannot answer "which command failed". The flag exists
            # for exactly this purpose (`cli.rs`, "Meant for audits and
            # benchmark runs"); `run_swebench.py` has always passed it.
            "--verbose-tool-input",
        ]
        # Only when the task declared servers: passing it otherwise would point
        # Codape at a path `configure` deliberately did not create.
        if self.mcp_servers:
            parts.extend(["--mcp-config", MCP_CONFIG_PATH])
        parts.extend(self._extra_args)
        return " ".join(shlex.quote(part) for part in parts)

    @with_prompt_template
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        rendered = self._render_instruction(instruction)
        # The instruction goes in as a file, not as an argv entry: it is
        # multi-line and can contain shell metacharacters, and a task prompt
        # is untrusted input as far as this adapter is concerned.
        local = self.logs_dir / "instruction.txt"
        local.parent.mkdir(parents=True, exist_ok=True)
        local.write_text(rendered, encoding="utf-8")
        await environment.upload_file(str(local), INSTRUCTION_PATH)

        # The exit status is recorded even when the command fails, so a run
        # that died is distinguishable from one that never started. `|| true`
        # keeps the shell returning 0; `exec_as_agent` raises on non-zero.
        command = (
            f"{self._command()} < {INSTRUCTION_PATH} > {STREAM_PATH} 2>&1; "
            f'rc=$?; printf "%s\\n" "$rc" > {EXIT_CODE_PATH}; '
            f"exit 0"
        )
        # With an inner `timeout` configured, this transport limit is
        # deliberately longer, so it is never what ends the run: if it fired
        # first the exit status would be lost and a slow run would look like a
        # crashed one. With no inner timeout there is nothing to outlive, so the
        # exec is left unbounded and Harbor's own timer is the only deadline —
        # the same arrangement `claude_code.py` uses.
        exec_timeout = (
            int(self._timeout_sec) + EXIT_GRACE_SEC
            if self._timeout_sec is not None
            else None
        )
        await self.exec_as_agent(
            environment,
            command=command,
            env=self._env(),
            timeout_sec=exec_timeout,
        )

        summary = await self._record(environment, context)

        # Codape answers the inner `timeout` with a `subtype: "interrupted"`
        # result frame (carrying the usage it had been billed). Returning
        # normally would record that truncated turn as the agent's answer, so
        # the trial would be graded and reported as a wrong answer rather than a
        # timeout — which is how a short ceiling on a long task turns into "the
        # model is weak". Raising Harbor's own exception type keeps the error
        # policy a real `AgentTimeoutError` gets (`should_grade=True`,
        # `has_valid_reward=True`), and the metrics and trajectory written above
        # survive because Harbor has kept the context object it passed in.
        #
        # Harbor's `_run_agent_phase` catches `asyncio.TimeoutError` — the base
        # class — and re-raises its own `AgentTimeoutError` with the task
        # budget in the message, so this message is replaced. Only the type
        # matters here, and that is what the policy is keyed on; the adapter's
        # own ceiling is still recorded in `context.metadata["subtype"]`.
        if self._timed_out(summary):
            raise AgentTimeoutError(
                f"Codape was stopped by its own {int(self._timeout_sec)}s "
                "timeout (--ak timeout_sec); the task's agent budget is larger"
            )

    def _timed_out(self, summary: dict | None) -> bool:
        """Whether the run ended because the adapter's own `timeout` fired.

        Only possible when `timeout_sec` is configured — without it the adapter
        sends no signal, and Harbor's timer is what stops a runaway run.
        """
        if self._timeout_sec is None:
            return False
        return (summary or {}).get("subtype") == "interrupted"

    async def _record(
        self, environment: BaseEnvironment, context: AgentContext
    ) -> dict | None:
        """Read the run back and write both the metrics and the trajectory.

        Everything here is best-effort with respect to the trial's verdict: a
        missing transcript must not turn a finished trial into a crashed one,
        so failures degrade to missing telemetry rather than raising. The
        parsed `result` frame is returned so the caller can tell a finished run
        from one its own timer cut short.
        """
        transcript = ""
        result = await environment.exec(
            command=f"cat {STREAM_PATH} 2>/dev/null || true"
        )
        if result.return_code == 0:
            transcript = result.stdout or ""

        summary = parse_result_summary(transcript)
        exit_result = await environment.exec(
            command=f"cat {EXIT_CODE_PATH} 2>/dev/null || true"
        )
        exit_code = _as_int((exit_result.stdout or "").strip())

        usage = (summary or {}).get("usage") or {}
        # `n_input_tokens` is documented as "including cache"; see
        # `prompt_tokens_from` for why that is a three-term sum.
        context.n_input_tokens = prompt_tokens_from(usage) or 0
        context.n_cache_tokens = usage.get("cache_read_input_tokens", 0)
        context.n_output_tokens = usage.get("output_tokens", 0)
        if (summary or {}).get("total_cost_usd") is not None:
            context.cost_usd = float(summary["total_cost_usd"])
        context.metadata = {
            "version": self._version,
            "exit_code": exit_code,
            "subtype": (summary or {}).get("subtype"),
            "is_error": (summary or {}).get("is_error"),
            "num_turns": (summary or {}).get("num_turns"),
            "duration_ms": (summary or {}).get("duration_ms"),
            "model_ms": (summary or {}).get("model_ms"),
            "tool_ms": (summary or {}).get("tool_ms"),
            "result_frames": (summary or {}).get("result_frames"),
        }

        # What the trajectory records as the model. Ask `_env()`, not
        # `self.model_name`: that attribute is Harbor's `-m` and is routinely
        # **null** for a custom agent — `harbor run` without one leaves
        # `agents[0].model_name` null, and Codape then falls back to the host's
        # `CODAPE_MODEL`. Reading `_env()` records the id the container was
        # actually launched with, which is the same resolution `_env()` applies
        # and so cannot drift from it.
        trajectory = build_trajectory(
            transcript,
            model_name=self._env().get("CODAPE_MODEL"),
            agent_version=self._version,
            instruction=self._render_instruction_placeholder(),
        )
        if trajectory is not None:
            self.logs_dir.mkdir(parents=True, exist_ok=True)
            (self.logs_dir / "trajectory.json").write_text(
                trajectory.model_dump_json(indent=2), encoding="utf-8"
            )
        return summary

    def _render_instruction_placeholder(self) -> str | None:
        # The rendered instruction is already on disk from `run`; read it back
        # so the trajectory's user step matches what was actually sent.
        path = self.logs_dir / "instruction.txt"
        try:
            return path.read_text(encoding="utf-8")
        except OSError:
            return None


def _read_pricing(path: str | None) -> dict[str, dict[str, float]]:
    """Read and validate a host-side ``pricing.json`` for the config home.

    Shape is Codape's own (`cost.rs`, ``load_pricing_from_path``): a map of
    model id to the four per-million rates. Validated here so a typo fails at
    agent construction — before a container is built and a task is paid for —
    rather than as a silently missing ``total_cost_usd`` at the end of a run.

    A key the four above do not name is carried through rather than dropped, so
    a table written for a newer Codape still round-trips intact to an older
    one. That is forward compatibility, not leniency: dropping it would let a
    future ``cache_write_per_million`` vanish silently. Whether Codape can read
    the extra key is Codape's call, and ``serde`` rejects an unknown field
    without ``deny_unknown_fields`` only when the struct declares it — so an
    older binary ignores what it does not know rather than failing the file.
    """
    if not path:
        return {}
    source = Path(str(path)).expanduser()
    try:
        raw = json.loads(source.read_text(encoding="utf-8"))
    except OSError as exc:
        raise FileNotFoundError(f"pricing file cannot be read: {source}") from exc
    except json.JSONDecodeError as exc:
        raise ValueError(f"pricing file is not JSON: {source}: {exc}") from exc
    if not isinstance(raw, dict):
        raise ValueError(f"pricing file must map model ids to rates: {source}")
    required = (
        "input_per_million",
        "output_per_million",
        "cache_read_per_million",
        "cache_creation_per_million",
    )
    table: dict[str, dict[str, float]] = {}
    for model, rates in raw.items():
        if not isinstance(rates, dict):
            raise ValueError(f"pricing entry for {model!r} must be an object: {source}")
        entry: dict[str, float] = {}
        for name, rate in rates.items():
            if not isinstance(rate, (int, float)) or isinstance(rate, bool):
                raise ValueError(
                    f"pricing entry for {model!r} has a non-numeric {name!r}: {source}"
                )
            try:
                entry[str(name)] = float(rate)
            except (OverflowError, ValueError) as exc:
                raise ValueError(
                    f"pricing entry for {model!r} has an unusable {name!r}: {source}"
                ) from exc
        missing = [name for name in required if name not in entry]
        if missing:
            raise ValueError(
                f"pricing entry for {model!r} is missing {', '.join(missing)}: {source}"
            )
        table[str(model)] = entry
    return table


def _as_int(value: str) -> int | None:
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def strip_provider_prefix(model_name: str, provider: str | None) -> str:
    """Drop a LiteLLM provider prefix the API would not recognise.

    Only a prefix naming **this adapter's own provider** is removed. A gateway
    can legitimately serve ``vendor/model`` ids, and so can Codape's
    OpenAI-compatible path; stripping those would break a working route, which
    is worse than the original bug. ``provider`` may be None (Harbor could not
    parse the name), in which case the literal ``anthropic/`` is still safe to
    drop — that spelling can only come from Harbor's own convention.

    Pure so ``check_harbor_agent.py`` can drive it without harbor installed.
    """
    if provider not in (None, "anthropic"):
        return model_name
    for prefix in ("anthropic/", "anthropic."):
        if model_name.startswith(prefix):
            return model_name[len(prefix):]
    return model_name


def parse_result_summary(text: str) -> dict | None:
    """Return the last Codape result frame, and totals over every frame.

    Identical semantics to ``codape_agent.parse_result_summary`` — see that
    function's docstring for why the two field families are counted
    differently. Kept as a copy rather than an import so the 0.2.18 adapter can
    be deleted without breaking this one.
    """
    last_result = None
    result_frames = 0
    usage_total: dict[str, int] | None = None
    total_cost: float | None = None
    timing_totals = dict.fromkeys(
        ("duration_ms", "num_turns", "model_ms", "tool_ms"), 0
    )
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            frame = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(frame, dict) or frame.get("type") != "result":
            continue
        last_result = frame
        result_frames += 1
        usage = frame.get("usage")
        if isinstance(usage, dict):
            usage_total = {
                "input_tokens": int(usage.get("input_tokens") or 0),
                "output_tokens": int(usage.get("output_tokens") or 0),
                "cache_read_input_tokens": int(
                    usage.get("cache_read_input_tokens")
                    or usage.get("cache_read_tokens")
                    or 0
                ),
                "cache_creation_input_tokens": int(
                    usage.get("cache_creation_input_tokens")
                    or usage.get("cache_creation_tokens")
                    or 0
                ),
            }
        for key in timing_totals:
            timing_totals[key] += int(frame.get(key) or 0)
        if frame.get("total_cost_usd") is not None:
            total_cost = float(frame["total_cost_usd"])
    if last_result is None:
        return None
    return {
        "is_error": bool(last_result.get("is_error")),
        "subtype": last_result.get("subtype"),
        "result_frames": result_frames,
        "usage": usage_total,
        "total_cost_usd": total_cost,
        **timing_totals,
    }


def build_trajectory(
    text: str,
    *,
    model_name: str | None = None,
    agent_version: str | None = None,
    instruction: str | None = None,
) -> Trajectory | None:
    """Convert a Codape stream-json transcript into an ATIF trajectory.

    Codape's protocol splits a turn across three frame kinds, and ATIF wants one
    ``Step`` per model turn with its tool calls and their results attached:

    * ``stream_event`` with a ``text_delta`` — the answer as it streamed;
    * ``stream_event`` with a ``thinking_delta`` — reasoning, kept separate;
    * ``assistant`` with a ``tool_use`` block — a call the model made;
    * ``user`` with a ``tool_result`` block — that call's output.

    There is no per-turn boundary frame to key on. ``result`` is emitted once,
    when the session ends — not once per turn — so flushing only on ``result``
    folded every turn of a 300-turn run into a single ``Step``: a real trial
    recorded 306 tool calls under one step, which makes the trajectory useless
    for exactly the per-turn analysis it exists to support.

    The boundary has to be inferred from the protocol's own ordering. A turn is
    ``[thinking][text][tool_use…][tool_result…]``, so once every accumulated
    call has its result the turn is complete, and whatever arrives next opens a
    new one. ``_turn_complete`` is that test; ``flush`` runs on it as well as on
    ``result`` and at end of stream.

    Returns ``None`` when the transcript holds no turns, because an empty
    trajectory is worse than none: Harbor would render an empty tab and a
    consumer could not tell it from a run that did nothing.

    This is a pure function of the text so it can be checked without Docker —
    the same reason ``check_reporting.py`` drives the parsers on synthetic
    transcripts.
    """
    text_parts: list[str] = []
    thinking_parts: list[str] = []
    tool_calls: list[ToolCall] = []
    results: dict[str, str] = {}
    saw_anything = False

    def flush() -> Step | None:
        nonlocal text_parts, thinking_parts, tool_calls, results
        if not (text_parts or thinking_parts or tool_calls):
            return None
        message = "".join(text_parts).strip()
        # `step_id` is assigned after assembly; ATIF requires sequential ids
        # starting at 1 and validates that on construction, so a placeholder
        # of 0 is rejected outright. The provisional value is the position.
        step = Step(
            step_id=len(steps) + 1,
            source="agent",
            message=message or "",
            reasoning_content=("".join(thinking_parts).strip() or None),
            tool_calls=tool_calls or None,
            observation=(
                Observation(
                    results=[
                        ObservationResult(
                            source_call_id=call.tool_call_id,
                            content=results.get(call.tool_call_id, ""),
                        )
                        for call in tool_calls
                    ]
                )
                if tool_calls
                else None
            ),
        )
        text_parts, thinking_parts, tool_calls = [], [], []
        # Results are cleared with the calls they belong to. Keeping them would
        # let a later turn satisfy `turn_complete` on a stale id and flush a
        # step whose observation is empty.
        results = {}
        return step

    steps: list[Step] = []
    if instruction:
        steps.append(Step(step_id=1, source="user", message=instruction))

    def turn_complete() -> bool:
        """True when the accumulated turn's tool cycle has finished.

        One turn is ``[thinking][text][tool_use…][tool_result…]``, so the turn
        ends at the frame that completes the *last* outstanding result — not at
        the frame after it. Waiting for a following frame would be wrong for a
        different reason in each direction:

        * flushing on any later content frame splits one streamed answer into a
          step per token, because ``text_delta`` arrives per token;
        * a turn that makes no calls (the model's final prose) has no tool
          cycle to close and is flushed by ``result`` or end-of-stream instead.

        Calls may be issued in parallel, so results can arrive in any order and
        ``all`` over the accumulated calls is what says the cycle is done.
        """
        return bool(tool_calls) and all(
            call.tool_call_id in results for call in tool_calls
        )

    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            frame = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(frame, dict):
            continue
        kind = frame.get("type")
        if kind == "stream_event":
            event = frame.get("event") or {}
            delta = event.get("delta") or {}
            if delta.get("type") == "text_delta":
                text_parts.append(str(delta.get("text") or ""))
                saw_anything = True
            elif delta.get("type") == "thinking_delta":
                thinking_parts.append(str(delta.get("thinking") or ""))
                saw_anything = True
        elif kind in ("assistant", "user"):
            content = ((frame.get("message") or {}).get("content")) or []
            if not isinstance(content, list):
                continue
            for block in content:
                if not isinstance(block, dict):
                    continue
                if block.get("type") == "tool_use":
                    tool_calls.append(
                        ToolCall(
                            tool_call_id=str(block.get("id") or ""),
                            function_name=str(block.get("name") or ""),
                            arguments=block.get("input") or {},
                        )
                    )
                    saw_anything = True
                elif block.get("type") == "tool_result":
                    results[str(block.get("tool_use_id") or "")] = _result_text(
                        block.get("content")
                    )
                    saw_anything = True
        elif kind == "result":
            pending = flush()
            if pending is not None:
                steps.append(pending)
        # The turn's tool cycle closed on this frame. Flushing here is what
        # turns a 300-turn session into 300 steps instead of one; `result` is
        # only the session's last frame and would collapse everything.
        if turn_complete():
            pending = flush()
            if pending is not None:
                steps.append(pending)

    pending = flush()
    if pending is not None:
        steps.append(pending)
    if not saw_anything and len(steps) <= (1 if instruction else 0):
        return None

    # ATIF requires step ids to start at 1 and stay sequential. Renumbering
    # here rather than while building keeps the assembly above readable.
    for index, step in enumerate(steps, start=1):
        step.step_id = index

    usage = parse_result_summary(text) or {}
    totals = usage.get("usage") or {}
    return Trajectory(
        schema_version="ATIF-v1.8",
        agent=Agent(
            name="codape",
            version=agent_version or "unknown",
            model_name=model_name,
            tool_definitions=None,
        ),
        steps=steps,
        # `final_metrics` is `FinalMetrics`, whose fields are `total_*` — not
        # the per-step `Metrics` (prompt_tokens/completion_tokens/...). Both
        # are validated on construction, so passing the wrong model is a hard
        # error rather than a silently empty table.
        final_metrics=FinalMetrics(
            total_prompt_tokens=prompt_tokens_from(totals),
            total_completion_tokens=totals.get("output_tokens"),
            # ATIF defines this as the *cache-hit* subset of `prompt_tokens`.
            # Cache creation is charged as input but is not a hit, so it stays
            # out of this field while remaining in `total_prompt_tokens`.
            total_cached_tokens=totals.get("cache_read_input_tokens"),
            total_cost_usd=usage.get("total_cost_usd"),
            total_steps=len(steps),
        )
        if totals
        else None,
    )


def prompt_tokens_from(usage: dict) -> int | None:
    """The billed input for one session: uncached + cache read + cache write.

    Anthropic's `input_tokens` excludes both cache fields; Codape's own
    `UsageSummary::prompt_tokens` adds all three, and this mirrors it so the
    numbers Harbor records agree with the ones Codape reports locally. Returns
    None only when the frame carried no usage at all — a zero is a real
    answer, and callers distinguish it from absent.

    Pure, so `check_harbor_agent.py` can drive it.
    """
    if not usage:
        return None
    return (
        int(usage.get("input_tokens") or 0)
        + int(usage.get("cache_read_input_tokens") or 0)
        + int(usage.get("cache_creation_input_tokens") or 0)
    )


def _result_text(content: object) -> str:
    """Flatten a `tool_result` content value to the text a human should read.

    Codape sends either a string or the text-plus-image array the provider
    receives. Images are dropped: ATIF's observation content is text, and a
    base64 blob in a trajectory is unreadable in the viewer.
    """
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = [
            str(block.get("text") or "")
            for block in content
            if isinstance(block, dict) and block.get("type") == "text"
        ]
        return "".join(parts)
    return ""
