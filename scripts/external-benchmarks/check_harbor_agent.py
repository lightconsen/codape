#!/usr/bin/env python3
"""Check the Harbor adapter's ATIF conversion without Docker or a model.

The Harbor adapter (`codape_harbor_agent.py`) declares
``capabilities = AgentCapabilities(atif=True)``, which is a promise that
``trajectory.json`` holds a valid ATIF document. Harbor renders that file in its
viewer and uses it to compare runs, so a malformed one is worse than absent.

The conversion is a pure function of the transcript precisely so this check can
exist: the only way to exercise the rest of the adapter is a paid benchmark, and
a defect found there costs tokens to discover. This drives `build_trajectory` on
synthetic transcripts and validates the result against Harbor's *own* models
when harbor is installed, falling back to plain structural assertions when it is
not (CI has no harbor, and must not install one for a 2-second check).

Usage:
    python3 -I scripts/external-benchmarks/check_harbor_agent.py
"""

from __future__ import annotations

import asyncio
import importlib.util
import json
import os
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
FAILURES: list[str] = []


def check(condition: bool, message: str) -> None:
    if not condition:
        FAILURES.append(message)


def stub_harbor() -> None:
    """Stand in for harbor's models with the fields the adapter sets.

    Deliberately not a permissive double: every attribute the adapter relies on
    is named here, so a rename in the adapter shows up as an AttributeError
    instead of silently passing. `model_fields`-style validation is skipped —
    that is what the real-harbor branch below is for.
    """

    class _Model:
        # No `model_dump_json`: the shape-only stub cannot reproduce nested
        # serialization faithfully, and a stub that quietly stringifies a
        # `Step` would make the structural checks below assert on garbage.
        # Checks against the stub therefore walk attributes; the real
        # JSON round trip is the harbor branch in `check_against_real_harbor`.
        def __init__(self, **fields):
            self.__dict__.update(fields)

    def make(name: str):
        return type(name, (_Model,), {})

    modules = {}
    for package in (
        "harbor",
        "harbor.agents",
        "harbor.agents.installed",
        "harbor.environments",
        "harbor.models",
        "harbor.models.agent",
        "harbor.trial",
    ):
        module = type(sys)(package)
        module.__path__ = []
        modules[package] = module
        sys.modules[package] = module

    capabilities = type(sys)("harbor.agents.capabilities")

    class AgentCapabilities(_Model):
        pass

    capabilities.AgentCapabilities = AgentCapabilities
    sys.modules[capabilities.__name__] = capabilities

    class _Connection:
        """What `BaseAgent.model_connection` yields, reduced to the fields read.

        The stub never resolves a connection, so `model_id` is None and the
        adapter has to fall back to its own prefix stripping — which is the
        branch worth exercising without harbor installed. The real resolution
        is covered by `check_against_real_harbor` when harbor is present.
        """

        def __init__(self):
            self.provider = None
            self.model_id = None
            self.env = {}

    installed = type(sys)("harbor.agents.installed.base")

    class BaseInstalledAgent:
        MODEL_CONNECTION = None

        def __init__(self, logs_dir=None, model_name=None, **kwargs):
            del kwargs
            self.logs_dir = logs_dir
            self.model_name = model_name

        @property
        def model_connection(self):
            return _Connection()

    def with_prompt_template(fn):
        return fn

    installed.BaseInstalledAgent = BaseInstalledAgent
    installed.with_prompt_template = with_prompt_template
    sys.modules[installed.__name__] = installed

    auth = type(sys)("harbor.agents.agent_auth_resolution")

    class ProviderAccess(_Model):
        pass

    class ModelConnectionSpec(_Model):
        def default_base_url(self, provider, api_format=None, **kwargs):
            del provider, api_format, kwargs
            return None

    auth.ProviderAccess = ProviderAccess
    auth.ModelConnectionSpec = ModelConnectionSpec
    sys.modules[auth.__name__] = auth

    api_format = type(sys)("harbor.models.model_api_format")

    class ModelApiFormat:
        ANTHROPIC_MESSAGES = "anthropic_messages"

    api_format.ModelApiFormat = ModelApiFormat
    sys.modules[api_format.__name__] = api_format

    environments = type(sys)("harbor.environments.base")

    class BaseEnvironment:
        pass

    environments.BaseEnvironment = BaseEnvironment
    sys.modules[environments.__name__] = environments

    context = type(sys)("harbor.models.agent.context")

    class AgentContext(_Model):
        pass

    context.AgentContext = AgentContext
    sys.modules[context.__name__] = context

    trajectories = type(sys)("harbor.models.trajectories")
    for name in (
        "Agent",
        "FinalMetrics",
        "Metrics",
        "Observation",
        "ObservationResult",
        "Step",
        "ToolCall",
        "Trajectory",
    ):
        setattr(trajectories, name, make(name))
    sys.modules[trajectories.__name__] = trajectories

    errors = type(sys)("harbor.trial.errors")

    class AgentTimeoutError(asyncio.TimeoutError):
        pass

    errors.AgentTimeoutError = AgentTimeoutError
    sys.modules[errors.__name__] = errors

    for name, module in modules.items():
        sys.modules[name] = module


def load_adapter():
    # Only stub when the real package is genuinely absent. If harbor *is*
    # installed, the stub would shadow it and the real-model round trip below
    # would silently never run — the check would pass while testing nothing.
    try:
        import harbor.models.trajectories  # noqa: F401
    except ImportError:
        stub_harbor()
    spec = importlib.util.spec_from_file_location(
        "codape_harbor_agent_under_test", ROOT / "terminal-bench" / "codape_harbor_agent.py"
    )
    if spec is None or spec.loader is None:
        raise SystemExit("cannot load codape_harbor_agent.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def transcript(*frames: dict) -> str:
    return "\n".join(json.dumps(frame) for frame in frames) + "\nnot json\n"


TEXT = {"type": "stream_event", "event": {"type": "content_block_delta",
                                          "delta": {"type": "text_delta", "text": "I will look."}}}
THINKING = {"type": "stream_event", "event": {"type": "content_block_delta",
                                              "delta": {"type": "thinking_delta", "thinking": "Need ls."}}}
TOOL_USE = {"type": "assistant", "message": {"content": [
    {"type": "tool_use", "id": "c1", "name": "Bash", "input": {"command": "ls"}}]}}
TOOL_RESULT = {"type": "user", "message": {"content": [
    {"type": "tool_result", "tool_use_id": "c1", "content": "a.txt"}]}}
RESULT = {"type": "result", "subtype": "success", "is_error": False,
          "duration_ms": 100, "num_turns": 2,
          "usage": {"input_tokens": 10, "output_tokens": 5,
                    "cache_read_input_tokens": 3, "cache_creation_input_tokens": 1}}
# What Codape writes when its own `timeout` sends SIGTERM: a closing result
# frame that still carries what the provider billed.
INTERRUPTED = {"type": "result", "subtype": "interrupted", "is_error": False,
               "duration_ms": 100, "num_turns": 1,
               "usage": {"input_tokens": 10, "output_tokens": 5}}


def check_model_env(module) -> None:
    """The model Harbor selects must reach Codape, in a spelling it forwards.

    Two separate defects lived here. `--model` was never applied at all (the
    adapter passed only host environment through), and Harbor's spelling is
    `anthropic/claude-sonnet-4-5`, which Codape forwards verbatim to the API.
    Either one alone produces a run that scores zero for reasons that look like
    a weak model.
    """
    agent = module.CodapeHarborAgent(
        logs_dir=Path("/tmp"), model_name="anthropic/claude-sonnet-4-5"
    )
    env = agent._env()
    check(env.get("CODAPE_MODEL") == "claude-sonnet-4-5",
          f"CODAPE_MODEL must carry the bare model id, got {env.get('CODAPE_MODEL')!r}")

    # A provider prefix that is not ours stays: a gateway may serve it.
    check(module.strip_provider_prefix("deepseek/deepseek-v4", "deepseek")
          == "deepseek/deepseek-v4",
          "a foreign provider prefix must survive")
    check(module.strip_provider_prefix("claude-sonnet-4-5", None) == "claude-sonnet-4-5",
          "an unprefixed model must be unchanged")

    # The capability/spec pairing is what makes `--base-url` and inference-host
    # allowlisting work; `base_url=True` with `MODEL_CONNECTION = None` raises
    # at construction time in Harbor's BaseAgent.
    check(module.CodapeHarborAgent.capabilities.base_url,
          "base_url capability is declared but the URL path is untested")
    spec = module.CodapeHarborAgent.MODEL_CONNECTION
    check(spec is not None, "capabilities.base_url=True requires a MODEL_CONNECTION")
    if spec is not None:
        check(spec.api_key_destination_names.get("default") == "CODAPE_API_KEY",
              "the key must land in a variable Codape reads first")
        check(spec.base_url_destination_names.get("default") == "CODAPE_BASE_URL",
              "the URL must land in a variable Codape reads first")


def check_build(module) -> None:
    traj = module.build_trajectory(
        transcript(THINKING, TEXT, TOOL_USE, TOOL_RESULT, RESULT),
        model_name="m",
        agent_version="v0.2.2",
        instruction="do the task",
    )
    check(traj is not None, "a transcript with turns must produce a trajectory")
    if traj is None:
        return
    check(traj.schema_version == "ATIF-v1.8",
          f"schema_version must be ATIF-v1.8, got {traj.schema_version}")
    steps = traj.steps
    check(len(steps) >= 2, f"expected a user step and an agent step, got {len(steps)}")
    check([s.step_id for s in steps] == list(range(1, len(steps) + 1)),
          f"step ids must be 1..n in order, got {[s.step_id for s in steps]}")
    check(steps[0].source == "user", "the first step must be the instruction")
    agent = next((s for s in steps if s.source == "agent"), None)
    check(agent is not None, "a transcript with tool calls must have an agent step")
    if agent is not None:
        check("I will look." in (agent.message or ""),
              f"the text delta must land in the agent step's message: {agent.message!r}")
        check("Need ls." in (agent.reasoning_content or ""),
              "the thinking delta must land in reasoning_content")
        calls = agent.tool_calls or []
        check(len(calls) == 1, f"expected one tool call, got {len(calls)}")
        if calls:
            check(calls[0].function_name == "Bash",
                  f"ATIF names the tool field function_name, got {calls[0].function_name!r}")
        results = (agent.observation.results if agent.observation else None) or []
        check(len(results) == 1, f"expected one observation result, got {len(results)}")
        if results:
            check(results[0].content == "a.txt",
                  f"the tool result must be attached to its call: {results[0].content!r}")

    # A transcript with nothing in it must not produce a document: Harbor would
    # render an empty tab, which reads as a run that did nothing.
    check(module.build_trajectory(transcript(RESULT)) is None,
          "a transcript with no turns must produce no trajectory")


def check_multi_turn(module) -> None:
    """One ``Step`` per model turn, not one for the whole session.

    Codape emits ``result`` once, when the session ends — never per turn. An
    adapter that flushed only on ``result`` therefore folded every turn into a
    single step: the first harbor trial recorded 306 tool calls under one step,
    300 turns' worth of trajectory in a document that could not be read. This
    is the case a single-turn fixture cannot catch, because with one turn
    "flush at the end" and "flush at each turn boundary" agree.

    The boundary has to be inferred. Three turns here, each a tool call and its
    result, with no frame separating them but the results themselves.
    """
    def use(cid, command):
        return {"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": cid, "name": "Bash",
             "input": {"command": command}}]}}

    def result(cid, text):
        return {"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": cid, "content": text}]}}

    traj = module.build_trajectory(
        transcript(
            THINKING, TEXT, use("c1", "ls"), result("c1", "a.txt"),
            THINKING, TEXT, use("c2", "cat a.txt"), result("c2", "hello"),
            THINKING, TEXT, use("c3", "pwd"), result("c3", "/app"),
            RESULT,
        ),
        instruction="do the task",
    )
    check(traj is not None, "multi-turn: production must not be None")
    if traj is None:
        return
    agent_steps = [s for s in traj.steps if s.source == "agent"]
    check(len(agent_steps) == 3,
          f"three tool turns must produce three agent steps, got {len(agent_steps)}")
    for index, step in enumerate(agent_steps, start=1):
        calls = step.tool_calls or []
        check(len(calls) == 1,
              f"step {index} must carry exactly its own call, got {len(calls)}")
        if not calls:
            continue
        check(calls[0].tool_call_id == f"c{index}",
              f"step {index} must carry call c{index}, got {calls[0].tool_call_id!r}")
        # A result left in the accumulator would attach this turn's call to a
        # previous turn's output, or leave the observation empty.
        observed = (step.observation.results if step.observation else None) or []
        check(len(observed) == 1,
              f"step {index} must carry its own observation, got {len(observed)}")
        if observed:
            check(observed[0].source_call_id == f"c{index}",
                  f"step {index} observation must match its call, got "
                  f"{observed[0].source_call_id!r}")
    check(traj.final_metrics is not None
          and traj.final_metrics.total_steps == len(traj.steps),
          "total_steps must still match the step count")

    # Parallel calls in one turn are one step, not one each: the boundary is
    # "the turn's cycle closed", not "a result arrived".
    parallel = module.build_trajectory(
        transcript(
            use("p1", "ls"), use("p2", "pwd"),
            result("p1", "a.txt"), result("p2", "/app"),
            RESULT,
        ),
        instruction="x",
    )
    check(parallel is not None, "parallel: production must not be None")
    if parallel is not None:
        steps = [s for s in parallel.steps if s.source == "agent"]
        check(len(steps) == 1,
              f"two parallel calls are one turn, got {len(steps)} steps")
        if steps:
            check(len(steps[0].tool_calls or []) == 2,
                  "both parallel calls must land in the one step")

    # The model's closing prose makes no call, so nothing closes a tool cycle
    # and only `result` or end-of-stream flushes it. It must still appear.
    closing = module.build_trajectory(
        transcript(use("c1", "ls"), result("c1", "a.txt"), THINKING, TEXT, RESULT),
        instruction="x",
    )
    check(closing is not None, "closing-prose: production must not be None")
    if closing is not None:
        steps = [s for s in closing.steps if s.source == "agent"]
        check(len(steps) == 2,
              f"tool turn plus closing prose must be two steps, got {len(steps)}")
        if len(steps) == 2:
            check("I will look." in (steps[1].message or ""),
                  f"the closing answer must be its own step: {steps[1].message!r}")
            check(not (steps[1].tool_calls or []),
                  "the closing step must make no calls")


def check_metrics(module) -> None:
    traj = module.build_trajectory(transcript(TEXT, RESULT), instruction="x")
    assert traj is not None
    metrics = traj.final_metrics
    check(metrics is not None, "a run with usage must carry final_metrics")
    if metrics is None:
        return
    # 10 uncached + 3 cache read + 1 cache write. `input_tokens` alone is what
    # Anthropic reports as uncached; Harbor's `n_input_tokens` is documented as
    # including cache, and Codape's own `prompt_tokens` adds all three. Using
    # `input_tokens` here was a real under-count of every cache write.
    check(metrics.total_prompt_tokens == 14,
          f"total_prompt_tokens must include both cache fields: {metrics.total_prompt_tokens}")
    check(metrics.total_completion_tokens == 5,
          f"total_completion_tokens: {metrics.total_completion_tokens}")
    # A cache *write* is billed as input but is not a cache hit, so it must not
    # land here — ATIF defines this as the hit subset.
    check(metrics.total_cached_tokens == 3,
          f"total_cached_tokens is the hit subset only: {metrics.total_cached_tokens}")
    check(metrics.total_steps == len(traj.steps),
          f"total_steps must match the step count: {metrics.total_steps}")

    # A frame with no usage is absent, not zero: 0 would read as a free run.
    check(module.prompt_tokens_from({}) is None,
          "no usage must yield None, not 0")
    check(module.prompt_tokens_from({"input_tokens": 0}) == 0,
          "a reported zero must stay 0")


def check_against_real_harbor(module) -> None:
    """When harbor is installed, validate through its own pydantic models.

    The stub branch proves the adapter's field *names*; this proves they are
    the names harbor accepts, including `extra="forbid"` on every model. Skipped
    entirely when the stub is in play — the stub's objects have no pydantic
    methods, and asserting through them would test the stub, not harbor.
    """
    try:
        from harbor.models.trajectories import Trajectory
    except ImportError:
        print("  (harbor not installed; structural checks only)")
        return
    if not hasattr(Trajectory, "model_validate_json"):
        # The stub is in play: it has no pydantic methods. Asserting through it
        # would test the stub, not harbor.
        print("  (harbor stubbed; structural checks only)")
        return
    traj = module.build_trajectory(
        transcript(THINKING, TEXT, TOOL_USE, TOOL_RESULT, RESULT),
        model_name="m",
        agent_version="v0.2.2",
        instruction="do the task",
    )
    check(traj is not None, "real-harbor: production must not be None")
    if traj is None:
        return
    text = traj.model_dump_json()
    try:
        back = Trajectory.model_validate_json(text)
    except Exception as exc:  # noqa: BLE001 - report, do not crash the check
        check(False, f"harbor rejected the trajectory: {exc}")
        return
    check(len(back.steps) >= 2, "real-harbor: steps must survive a round trip")
    print(f"  validated against harbor: {len(back.steps)} steps, "
          f"{len(text)} bytes")


class _ExecStub:
    """Stands in for the environment, recording the budget the run asks for.

    `exec_as_agent` is a method on the *agent* in Harbor, not on the
    environment, so `bind` attaches it here; the rest of the calls in `run` and
    `_record` go to the environment. Both are recorded, because a timeout on the
    reads instead of the run would be a different defect.
    """

    def __init__(self, files: dict[str, str] | None = None):
        self.files = files or {}
        self.run_timeouts: list[int | None] = []
        self.exec_calls: list[str] = []

    def _result(self, command: str):
        stdout = next(
            (text for name, text in self.files.items() if name in command), ""
        )

        class _Result:
            return_code = 0
            stderr = ""

        result = _Result()
        result.stdout = stdout
        return result

    async def exec(self, command, *_args, **_kwargs):
        self.exec_calls.append(command)
        return self._result(command)

    async def upload_file(self, *_args, **_kwargs):
        return None

    def bind(self, agent) -> None:
        async def exec_as_agent(_environment, command, *, timeout_sec=None, **_kw):
            self.exec_calls.append(command)
            self.run_timeouts.append(timeout_sec)
            return self._result(command)

        async def exec_as_root(_environment, command, *_args, **_kw):
            return await self.exec(command)

        agent.exec_as_agent = exec_as_agent
        agent.exec_as_root = exec_as_root


def _agent(module, **kwargs):
    return module.CodapeHarborAgent(
        logs_dir=Path(tempfile.mkdtemp(prefix="codape-harbor-check-")),
        model_name="m",
        **kwargs,
    )


def check_timeout(module) -> None:
    """The adapter must not end a run Harbor was still willing to pay for.

    Every task in `terminal-bench/terminal-bench@4.1.0` declares an 8-hour agent
    budget. A fixed ceiling shorter than that ends the run early and reports
    unfinished work as a wrong answer instead of a timeout — which is what the
    previous hardcoded 1740 s would have done on all 66. The default therefore
    imposes no ceiling at all, and a configured one has to leave the exec
    transport room to record the exit status after it fires.
    """
    default = _agent(module)
    command = default._command()
    check("timeout --signal" not in command,
          f"the default must impose no wall clock of its own, got {command!r}")
    check(default._timed_out({"subtype": "interrupted"}) is False,
          "with no configured timeout an interrupted frame cannot be the adapter's doing")

    configured = _agent(module, timeout_sec="28800")
    command = configured._command()
    check("timeout --signal=TERM --kill-after=30 28800 " in command,
          f"a configured timeout must reach the command, got {command!r}")
    # `timeout_sec` has to be a number by the time it reaches the command: the
    # CLI JSON-parses `--ak` values, but a non-JSON one arrives as a string and
    # `int()` on the first real run would raise.
    check(configured._timeout_sec == 28800.0,
          f"the CLI passes agent kwargs as strings: {configured._timeout_sec!r}")
    check(configured._timed_out({"subtype": "interrupted"}) is True,
          "an interrupted frame is a timeout when a timeout was configured")
    check(configured._timed_out({"subtype": "success"}) is False,
          "only the interrupted subtype reads as a timeout")
    check(configured._timed_out(None) is False,
          "a run with no result frame is not a timeout")

    stub = _ExecStub()
    stub.bind(configured)
    asyncio.run(configured.run("do it", stub, module.AgentContext()))
    check(len(stub.run_timeouts) == 1, f"expected one run exec, got {len(stub.run_timeouts)}")
    if stub.run_timeouts:
        budget = stub.run_timeouts[0]
        check(budget is not None and budget > 28800,
              f"the run exec must outlive the inner timeout, got {budget}")

    # With no ceiling the exec is unbounded, so Harbor's timer is the only
    # deadline. An arbitrary transport limit here would be the same defect, one
    # layer down: it would end the run without the exit status being recorded.
    stub = _ExecStub()
    stub.bind(default)
    asyncio.run(default.run("do it", stub, module.AgentContext()))
    if stub.run_timeouts:
        check(stub.run_timeouts[0] is None,
              f"no configured timeout means no exec timeout, got {stub.run_timeouts[0]}")


def check_install_prerequisites(module) -> None:
    """`install` must not assume the task image carries a downloader.

    The first task this adapter was run against — `cargo-flight-dispatch`, base
    `python:3.13-slim-bookworm` — has no `curl`, so the release step exited 127
    before any task work began. Only a real container shows that, and the
    failure looks like an agent that could not start. Harbor's own
    `ensure_system_dependencies` installs through the image's package manager;
    the reference implementation calls it before it curls anything.
    """

    agent = _agent(module, version="v0.2.2")
    calls: list[tuple] = []

    async def ensure_system_dependencies(_environment, dependencies):
        calls.append(tuple(dependencies))

    agent.ensure_system_dependencies = ensure_system_dependencies
    stub = _ExecStub()
    stub.bind(agent)
    asyncio.run(agent.install(stub))

    check(
        calls == [("curl", "ca_certificates", "xz")],
        f"install must ensure its download prerequisites first, got {calls}",
    )
    download = next(
        (c for c in stub.exec_calls if "releases/download" in c), ""
    )
    check(
        bool(download),
        "the release download should run after the prerequisites are in place",
    )
    # A single truncated stream must not fail a whole trial: the first real run
    # died with `curl: (18) HTTP/2 stream 1 was not closed cleanly`. The retry
    # has to be in the download command itself, not assumed of the host.
    check(
        "--retry" in download and "--retry-all-errors" in download,
        "the release download must retry a truncated transfer",
    )


def check_timed_out_raises(module) -> None:
    """An interrupted run must raise Harbor's timeout, not return an answer.

    A returned run is a graded run: Harbor records it as the agent's answer and
    it counts against the score. Only the exception type carries the error
    policy that says "this trial timed out".
    """
    agent = _agent(module, timeout_sec="28800")
    stub = _ExecStub({"codape-stream.jsonl": transcript(INTERRUPTED)})
    stub.bind(agent)
    try:
        asyncio.run(agent.run("do it", stub, module.AgentContext()))
    except module.AgentTimeoutError:
        return
    except Exception as exc:  # noqa: BLE001 - the type is the whole point
        check(False, f"an interrupted run raised {type(exc).__name__}, not AgentTimeoutError")
        return
    check(False, "an interrupted run returned normally instead of raising a timeout")


def check_trajectory_model_name(module) -> None:
    """The trajectory must name the model that actually ran, not Harbor's `-m`.

    `harbor run` without `-m` leaves `agents[0].model_name` **null** for a
    custom agent — that is what the first paid trial's `config.json` held — and
    Codape then falls back to the host's `CODAPE_MODEL`. `_record` used to pass
    `self.model_name` straight into `build_trajectory`, so the ATIF document of
    that run recorded `agent.model_name: null`: the one field Harbor uses to
    label a run, lost, in a document that costs real tokens to produce.

    Asserts on the **argument at the call site**, by intercepting
    `build_trajectory`. Reading the written file would need a real pydantic
    `Trajectory` (`model_dump_json`), and the stub deliberately has none, so
    this check must run without harbor — it is the one that catches a defect
    found on a paid run.
    """
    real = module.build_trajectory
    recorded: list[str | None] = []

    def capture(text, **kwargs):
        recorded.append(kwargs.get("model_name"))
        return None  # nothing to write, which is also a path `_record` handles

    module.build_trajectory = capture
    try:
        model = "deepseek-v4.1-flash"
        os.environ["CODAPE_MODEL"] = model
        try:
            agent = module.CodapeHarborAgent(
                logs_dir=Path(tempfile.mkdtemp(prefix="codape-harbor-model-")),
                model_name=None,
            )
            stub = _ExecStub({"codape-stream.jsonl": transcript(TEXT, RESULT)})
            stub.bind(agent)
            asyncio.run(agent.run("do it", stub, module.AgentContext()))
        finally:
            os.environ.pop("CODAPE_MODEL", None)

        check(recorded == [model],
              f"with no Harbor -m the trajectory must record the model the "
              f"container ran ({model!r}), got {recorded!r}")

        # The other half of the same choice: Harbor's `-m` still wins when it
        # exists, so the fix cannot silently start ignoring an explicit pick.
        recorded.clear()
        agent = module.CodapeHarborAgent(
            logs_dir=Path(tempfile.mkdtemp(prefix="codape-harbor-model-explicit-")),
            model_name="anthropic/claude-sonnet-4-5",
        )
        stub = _ExecStub({"codape-stream.jsonl": transcript(TEXT, RESULT)})
        stub.bind(agent)
        asyncio.run(agent.run("do it", stub, module.AgentContext()))
        check(recorded == ["claude-sonnet-4-5"],
              f"an explicit Harbor model must still win, got {recorded!r}")
    finally:
        module.build_trajectory = real


def main() -> int:
    module = load_adapter()
    print("harbor adapter: ATIF conversion")
    check_model_env(module)
    check_build(module)
    check_multi_turn(module)
    check_metrics(module)
    check_timeout(module)
    check_install_prerequisites(module)
    check_timed_out_raises(module)
    check_trajectory_model_name(module)
    check_against_real_harbor(module)
    if FAILURES:
        print(f"\n{len(FAILURES)} failure(s):")
        for failure in FAILURES:
            print(f"  - {failure}")
        return 1
    print("  ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
