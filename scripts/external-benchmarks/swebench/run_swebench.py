#!/usr/bin/env python3
"""Run Codape on a subset of SWE-bench and grade it with the official harness.

The runner has two phases:

1. Generation. For each selected instance it pulls the instance's official
   SWE-bench image, starts a throwaway container with the repository checked out
   at ``base_commit`` in ``/testbed``, installs Codape, and runs the headless
   agent on the issue text. The working-tree diff is captured as a prediction.
2. Evaluation. The official ``swebench`` harness applies each prediction to a
   fresh copy of the image, runs the dataset's tests, and writes a report.

Everything the benchmark needs is pinned in this file. Keys are never written to
disk: Codape receives them through the environment of the ``docker exec``
client, and ``--dry-run`` prints variable names only.

This is deliberately *not* part of normal CI. It needs Docker, network, tens of
gigabytes of images, and paid model calls. See ../README.md.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

REPO_ROOT = Path(__file__).resolve().parents[3]

# ---------------------------------------------------------------------------
# Pins. Change these together, and only on purpose.
# ---------------------------------------------------------------------------

SWEBENCH_VERSION = "5.0.2"

# Dataset repositories and the exact Hub revision each alias resolves to. The
# revision is what makes the subset (and therefore the score) reproducible even
# if the dataset owner later edits the default branch.
DATASETS = {
    "verified": {
        "repo": "SWE-bench/SWE-bench_Verified",
        "revision": "78f471bf655a3137b2e8a75af1501690ec009ec3",
        "file": "data/test-00000-of-00001.parquet",
        "instances": 500,
    },
    "lite": {
        "repo": "SWE-bench/SWE-bench_Lite",
        "revision": "b0dde1093fe417d83b7184254edf8199c1f0dff5",
        "file": "data/test-00000-of-00001.parquet",
        "instances": 300,
    },
    "multilingual": {
        "repo": "SWE-bench/SWE-bench_Multilingual",
        "revision": "846e647b9f33c0b51b739d005d13d85493c9af09",
        "file": "data/test-00000-of-00001.parquet",
        "instances": 300,
    },
}

# swebench 5.x is data-driven: every dataset row carries the evaluation script,
# log parser, image, and eval type, so a non-Python variant needs no
# runner-side MAP_REPO_VERSION_TO_SPECS table. This table is the one piece the
# runner still owns: it labels a row's parser with a language and rejects a
# pinned revision that names a parser the runner does not know. The values were
# taken from the pinned Multilingual rows and checked against the harness's
# PARSER_REGISTRY.
MULTILINGUAL_LANGUAGE_BY_PARSER = {
    "parse_log_ant": "Java",
    "parse_log_cargo": "Rust",
    "parse_log_doctest": "C++",
    "parse_log_googletest": "C++",
    "parse_log_gotest": "Go",
    "parse_log_gradle_custom": "Java",
    "parse_log_immutable_js": "JavaScript/TypeScript",
    "parse_log_jekyll": "Ruby",
    "parse_log_jest": "JavaScript/TypeScript",
    "parse_log_jq": "C",
    "parse_log_karma": "JavaScript/TypeScript",
    "parse_log_maven": "Java",
    "parse_log_micropython_test": "Python",
    "parse_log_phpunit": "PHP",
    "parse_log_redis": "C",
    "parse_log_rspec_transformed_json": "Ruby",
    "parse_log_ruby_unit": "Ruby",
    "parse_log_tap": "JavaScript/TypeScript",
    "parse_log_vitest": "JavaScript/TypeScript",
}
MULTILINGUAL_LANGUAGES = tuple(sorted(set(MULTILINGUAL_LANGUAGE_BY_PARSER.values())))

# Codape release installed inside the container when no --codape-binary is
# given. v0.2.2 is the first release that reports usage and where a turn's
# time went in its `result` frames (v0.2.0 and v0.2.1 predate both), and it
# still publishes `codape-*` archives — v0.1.3 and older ship `coderex-*`, so
# the installer below (which asks for `codape-<target>.tar.gz`) 404s on them.
CODAPE_RELEASE_VERSION = "v0.2.2"
CODAPE_TARGET = "x86_64-unknown-linux-musl"

# Variables forwarded from the host into Codape inside the container. The
# values are taken from the host environment; no key ever appears in a tracked
# file or in a process argument (only the variable *name* is passed to docker).
PASSTHROUGH_ENV = (
    "ANTHROPIC_API_KEY",
    # Claude Code and this repository's own test harnesses export the token
    # under AUTH_TOKEN; Codape accepts either spelling as the API key.
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
    # Claude Code reads gateway headers from this spelling; the runner
    # normalizes CODAPE_CUSTOM_HEADERS into it for the claude-code arm.
    "ANTHROPIC_CUSTOM_HEADERS",
    "CODAPE_API_KEY",
    "CODAPE_AUTH_TOKEN",
    "CODAPE_BASE_URL",
    "CODAPE_MODEL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_MODEL",
    "CODAPE_CUSTOM_HEADERS",
    # Codape assumes a conservative 32K window for a model it does not know.
    # A model with a larger real window then spends most of the task
    # compacting instead of working.
    "CODAPE_CONTEXT_WINDOW",
    # Raises the per-request output ceiling, which a reasoning model can
    # otherwise spend entirely on thinking and end the run having emitted
    # nothing. A raised value is clamped to half the model's context window.
    "CODAPE_MAX_TOKENS",
    # A slow reasoning model needs more than Codape's 120 s default per
    # request; without this the run aborts on the first long completion.
    "CODAPE_REQUEST_TIMEOUT_SECS",
)

API_KEY_ENV = (
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CODAPE_API_KEY",
    "CODAPE_AUTH_TOKEN",
    "OPENAI_API_KEY",
)
MODEL_ENV = ("ANTHROPIC_MODEL", "CODAPE_MODEL", "OPENAI_MODEL")

DEFAULT_AGENT_TIMEOUT = 1800
DEFAULT_EVAL_TIMEOUT = 1800
DEFAULT_MAX_TURNS = 300
DEFAULT_CONTEXT_TOKENS = 131072
# Generation is one container per instance and the instances are independent,
# so this is pure throughput. It is 1 by default because each container is a
# full repository image plus a model conversation — a 24-instance run at 4 is
# four model streams and four multi-GB images resident at once. Raise it with
# --n-concurrent as far as RAM and the provider's rate limit allow.
DEFAULT_N_CONCURRENT = 1
DEFAULT_N_ATTEMPTS = 1

CONTAINER_WORKDIR = "/testbed"
CONTAINER_CONFIG_DIR = "/tmp/codape-config"
CONTAINER_CLAUDE_CONFIG_DIR = "/tmp/claude-config"

# The exact patch-extraction script. It is also exercised by the local
# patch-format test: `git add -A` makes untracked files appear as additions,
# and `--binary` keeps binary changes applicable. Diffing the index against the
# instance's base commit also captures changes even if the agent commits.
#
# Some SWE-bench images check out every file as executable *and* leave the
# index recording that mode, so a one-hunk fix becomes thousands of
# `old mode 100644 / new mode 100755` entries. Reset the index to the base
# commit (mixed reset: the worktree is untouched) before staging, and run both
# the add and the diff with `core.fileMode=false`. Without the reset, the
# executable bits are already in the index; without the add setting, staging
# would put them back.
PATCH_SCRIPT = r"""set -eu
cd /testbed
rm -rf .codape
git reset -q __BASE__
git -c core.fileMode=false add -A -- . 2>/dev/null || git -c core.fileMode=false add -A
git -c core.fileMode=false -c core.quotepath=false diff --cached --no-ext-diff --binary __BASE__
"""

# Installs the pinned Linux release inside the container. The checksum is
# verified against the release's SHA256SUMS; a failed download or mismatch
# aborts the run instead of executing an unverified binary.
INSTALL_SCRIPT = r"""set -eu
VERSION="__VERSION__"
TARGET="__TARGET__"
BIN="/usr/local/bin/codape"
if [ -x "$BIN" ]; then
    "$BIN" --version
    exit 0
fi
BASE="https://github.com/lightconsen/codape/releases/download/$VERSION"
ARCHIVE="codape-$TARGET.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$2" "$1"
    elif command -v python3 >/dev/null 2>&1; then
        python3 -c 'import sys, urllib.request; urllib.request.urlretrieve(sys.argv[1], sys.argv[2])' "$1" "$2"
    else
        echo "no curl, wget, or python3 available to download Codape" >&2
        exit 1
    fi
}

fetch "$BASE/$ARCHIVE" "$TMP/$ARCHIVE"
fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS"
EXPECTED="$(awk -v name="$ARCHIVE" '$2 == name { print $1; exit }' "$TMP/SHA256SUMS")"
if [ -z "$EXPECTED" ]; then
    echo "SHA256SUMS has no entry for $ARCHIVE" >&2
    exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL="$(sha256sum "$TMP/$ARCHIVE" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL="$(shasum -a 256 "$TMP/$ARCHIVE" | awk '{ print $1 }')"
elif command -v python3 >/dev/null 2>&1; then
    ACTUAL="$(python3 -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$TMP/$ARCHIVE")"
else
    echo "no sha256 tool available to verify the Codape release" >&2
    exit 1
fi
if [ "$ACTUAL" != "$EXPECTED" ]; then
    echo "checksum mismatch for $ARCHIVE: expected $EXPECTED, got $ACTUAL" >&2
    exit 1
fi
tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
cp "$TMP/codape-$TARGET" "$BIN"
chmod 0755 "$BIN"
"$BIN" --version
"""


class BenchError(RuntimeError):
    """A user-facing benchmark failure."""


# ---------------------------------------------------------------------------
# Small pure helpers. These are intentionally importable without third-party
# packages so --help/--dry-run work in a bare Python installation.
# ---------------------------------------------------------------------------


def utc_stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def resolve_cache_dir(explicit: str | None) -> Path:
    if explicit:
        return Path(explicit).expanduser().resolve()
    env = os.environ.get("CODAPE_BENCH_CACHE")
    if env:
        return Path(env).expanduser().resolve()
    return Path.home() / ".cache" / "codape-external-benchmarks"


def dataset_spec(alias: str, revision: str | None) -> dict[str, str]:
    if alias not in DATASETS:
        raise BenchError(f"unknown dataset {alias!r}; choose one of: {', '.join(DATASETS)}")
    spec = DATASETS[alias]
    return {
        "alias": alias,
        "repo": spec["repo"],
        "revision": revision or spec["revision"],
        "file": spec["file"],
    }


def dataset_url(spec: dict[str, str]) -> str:
    return (
        f"https://huggingface.co/datasets/{spec['repo']}"
        f"/resolve/{spec['revision']}/{spec['file']}"
    )


def dataset_cache_path(cache_dir: Path, spec: dict[str, str]) -> Path:
    safe_repo = spec["repo"].replace("/", "__")
    return cache_dir / "datasets" / safe_repo / spec["revision"] / Path(spec["file"]).name


def select_instances(rows: list[dict[str, Any]], instance_ids: list[str] | None, limit: int | None) -> list[dict[str, Any]]:
    """Return the deterministic subset: named IDs in order, or sorted first N."""
    if instance_ids:
        by_id = {row["instance_id"]: row for row in rows}
        missing = [iid for iid in instance_ids if iid not in by_id]
        if missing:
            raise BenchError(
                "instance(s) not present in the pinned dataset: " + ", ".join(missing)
            )
        return [by_id[iid] for iid in instance_ids]
    # Sorting makes "first N" independent of Hub row order and of the parquet
    # writer. That is the reproducible subset promised by --limit.
    ordered = sorted(rows, key=lambda row: row["instance_id"])
    return ordered[: (1 if limit is None else limit)]


def language_for(instance: dict[str, Any], alias: str) -> str:
    """Return the language recorded for an instance, or reject an unknown one.

    Verified and Lite are Python-only. Multilingual names a parser per row; the
    table above turns it into a language and doubles as a contract check.
    """
    if alias != "multilingual":
        return "Python"
    parser = instance.get("log_parser")
    try:
        return MULTILINGUAL_LANGUAGE_BY_PARSER[parser]
    except KeyError:
        raise BenchError(
            f"{instance.get('instance_id', '<unknown>')}: multilingual row names "
            f"log_parser {parser!r}, which this runner does not map to a language; "
            "update MULTILINGUAL_LANGUAGE_BY_PARSER for the pinned revision"
        ) from None


def validate_instances(rows: list[dict[str, Any]], alias: str) -> None:
    """Check the columns the data-driven 5.x harness will read.

    These failures otherwise surface after the image pull and a paid model
    call, or inside the official evaluator with a less obvious message.
    """
    from swebench.harness.log_parsers import PARSER_REGISTRY  # inside the venv

    for row in rows:
        instance_id = row.get("instance_id") or "<missing instance_id>"
        for field in ("image", "base_commit", "problem_statement"):
            if not row.get(field):
                raise BenchError(f"{instance_id}: dataset row has no {field} field")
        if not str(row.get("eval_script") or "").strip():
            raise BenchError(
                f"{instance_id}: dataset row has no eval_script; "
                f"swebench=={SWEBENCH_VERSION} grades from that column"
            )
        parser = row.get("log_parser")
        if not parser:
            raise BenchError(f"{instance_id}: dataset row has no log_parser")
        if parser not in PARSER_REGISTRY:
            raise BenchError(
                f"{instance_id}: dataset row names log_parser {parser!r}, "
                f"which swebench=={SWEBENCH_VERSION} does not implement"
            )
        language_for(row, alias)


def build_prompt(instance: dict[str, Any], extra_instructions: str | None = None) -> str:
    base_commit = str(instance.get("base_commit", ""))[:12]
    prompt = f"""You are solving a real software engineering issue in the repository at /testbed.

Instance: {instance['instance_id']}
Repository: {instance.get('repo', 'unknown')} @ {base_commit}
Version: {instance.get('version', 'unknown')}

Resolve the issue by editing the source code in /testbed. Make the smallest
correct change you can, and leave the working tree modified. Do not create
commits. Do not try to look up the issue or its reference patch online.

--- ISSUE ---
{instance.get('problem_statement', '').strip()}
--- END ISSUE ---
"""
    if extra_instructions:
        prompt += "\n" + extra_instructions.strip() + "\n"
    return prompt


def parse_stream_json_summary(text: str) -> dict[str, Any]:
    """Aggregate a Codape stream-json transcript.

    Codape writes one ``result`` frame per continuation segment. The two
    families of fields are counted differently, so this is not a plain sum of
    everything:

    * ``duration_ms``, ``num_turns``, ``model_ms`` and ``tool_ms`` describe the
      submit that wrote the frame — summed over the run.
    * ``usage`` and ``total_cost_usd`` are session totals at the moment the
      frame was written (`attach_usage_to_frame` in `crates/cli/src/repl.rs`),
      so the last frame carrying them is the whole run. Summing those would
      count the first segment once per segment.

    Nothing here is invented: a field is ``None`` when no frame carried it,
    which is what an older binary's transcript looks like.
    """
    duration_ms = 0
    num_turns = 0
    model_ms = 0
    tool_ms = 0
    usage: dict[str, int] | None = None
    cost: float | None = None
    frames = 0
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
        frames += 1
        duration_ms += int(frame.get("duration_ms") or 0)
        num_turns += int(frame.get("num_turns") or 0)
        model_ms += int(frame.get("model_ms") or 0)
        tool_ms += int(frame.get("tool_ms") or 0)
        frame_usage = frame.get("usage")
        if isinstance(frame_usage, dict):
            usage = {
                "input_tokens": int(frame_usage.get("input_tokens") or 0),
                "output_tokens": int(frame_usage.get("output_tokens") or 0),
                "cache_read_input_tokens": int(
                    frame_usage.get("cache_read_input_tokens")
                    or frame_usage.get("cache_read_tokens")
                    or 0
                ),
                "cache_creation_input_tokens": int(
                    frame_usage.get("cache_creation_input_tokens")
                    or frame_usage.get("cache_creation_tokens")
                    or 0
                ),
            }
        if frame.get("total_cost_usd") is not None:
            cost = float(frame["total_cost_usd"])
    return {
        "result_frames": frames,
        "usage": usage,
        "total_cost_usd": cost,
        "duration_ms": duration_ms,
        "num_turns": num_turns,
        "model_ms": model_ms,
        "tool_ms": tool_ms,
    }


def parse_stream_json_usage(text: str) -> dict[str, Any] | None:
    """Extract cumulative usage/cost from Codape stream-json output, if any."""
    summary = parse_stream_json_summary(text)
    usage = summary["usage"]
    if usage is None:
        return None
    usage = dict(usage)
    usage["total_cost_usd"] = summary["total_cost_usd"]
    return usage


def result_frame(text: str) -> dict[str, Any] | None:
    """Return the last stream-json result frame, if the run produced one."""
    last = None
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            frame = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(frame, dict) and frame.get("type") == "result":
            last = frame
    return last


def looks_like_linux_elf(path: Path) -> bool:
    try:
        with path.open("rb") as handle:
            return handle.read(4) == b"\x7fELF"
    except OSError:
        return False


def safe_name(value: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9._-]+", "-", value.strip()).strip("-")
    return cleaned or "codape"


def model_label() -> str:
    for name in MODEL_ENV:
        value = os.environ.get(name)
        if value:
            return safe_name(value)
    return "default"


def prediction_model_name(explicit: str | None) -> str:
    if explicit:
        return explicit
    return f"codape-{model_label()}"


def read_predictions(path: Path) -> dict[str, dict[str, Any]]:
    predictions: dict[str, dict[str, Any]] = {}
    if not path.exists():
        raise BenchError(f"predictions file does not exist: {path}")
    with path.open(encoding="utf-8") as handle:
        for lineno, line in enumerate(handle, 1):
            line = line.strip()
            if not line:
                continue
            try:
                prediction = json.loads(line)
            except json.JSONDecodeError as exc:
                raise BenchError(f"{path}:{lineno}: invalid JSON: {exc}") from exc
            for field in ("instance_id", "model_name_or_path", "model_patch"):
                if field not in prediction:
                    raise BenchError(f"{path}:{lineno}: missing {field!r}")
            predictions[prediction["instance_id"]] = prediction
    return predictions


def image_platform(image: str) -> str | None:
    # SWE-bench publishes linux/amd64 images; the dataset names them x86_64.
    return "linux/amd64" if "x86_64" in image else None


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


def selected_env_names() -> list[str]:
    names = [name for name in PASSTHROUGH_ENV if os.environ.get(name)]
    # Both harnesses must see the same gateway headers.  Codape reads
    # CODAPE_CUSTOM_HEADERS; Claude Code reads ANTHROPIC_CUSTOM_HEADERS.
    # Forwarding the name lets agent_child_env() supply whichever spelling the
    # host set, without putting the value in a docker argument.
    if os.environ.get("CODAPE_CUSTOM_HEADERS") or os.environ.get("ANTHROPIC_CUSTOM_HEADERS"):
        for name in ("ANTHROPIC_CUSTOM_HEADERS", "CODAPE_CUSTOM_HEADERS"):
            if name not in names:
                names.append(name)
    return names


def agent_child_env() -> dict[str, str]:
    """Environment for the docker-exec client, never for a command argument."""
    env = os.environ.copy()
    headers = env.get("ANTHROPIC_CUSTOM_HEADERS") or env.get("CODAPE_CUSTOM_HEADERS")
    if headers:
        env["ANTHROPIC_CUSTOM_HEADERS"] = headers
        env["CODAPE_CUSTOM_HEADERS"] = headers
    window = env.get("CODAPE_CONTEXT_WINDOW")
    if window and not env.get("CLAUDE_CODE_MAX_CONTEXT_TOKENS"):
        env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"] = window
    return env


def claude_binary() -> Path:
    found = shutil.which("claude")
    if not found:
        raise BenchError("claude CLI not found on PATH; install Claude Code or use --agent codape")
    binary = Path(found).resolve()
    if not binary.is_file():
        raise BenchError(f"claude CLI is not a regular file: {binary}")
    return binary


def require_api_key() -> None:
    if not any(os.environ.get(name) for name in API_KEY_ENV):
        raise BenchError(
            "no model API key found in the environment. Set one of "
            + ", ".join(API_KEY_ENV)
            + " (and optionally ANTHROPIC_BASE_URL / ANTHROPIC_MODEL)."
        )


def venv_python(cache_dir: Path) -> Path:
    return cache_dir / f"venv-swebench-{SWEBENCH_VERSION}" / "bin" / "python"


def venv_install_commands(cache_dir: Path) -> list[list[str]]:
    venv = venv_python(cache_dir).parent.parent
    if shutil.which("uv"):
        return [
            ["uv", "venv", "--python", "3.12", str(venv)],
            ["uv", "pip", "install", "--python", str(venv_python(cache_dir)), f"swebench=={SWEBENCH_VERSION}", "pyarrow"],
        ]
    return [
        [sys.executable, "-m", "venv", str(venv)],
        [str(cache_dir / f"venv-swebench-{SWEBENCH_VERSION}" / "bin" / "pip"), "install", "--upgrade", "pip"],
        [str(cache_dir / f"venv-swebench-{SWEBENCH_VERSION}" / "bin" / "pip"), "install", f"swebench=={SWEBENCH_VERSION}", "pyarrow"],
    ]


def venv_is_ready(cache_dir: Path) -> bool:
    py = venv_python(cache_dir)
    if not py.exists():
        return False
    probe = (
        "import pyarrow, swebench\n"
        "from importlib.metadata import version\n"
        f"assert version('swebench') == '{SWEBENCH_VERSION}', version('swebench')\n"
    )
    result = subprocess.run([str(py), "-I", "-c", probe], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=False)
    return result.returncode == 0


def ensure_venv(cache_dir: Path) -> None:
    if venv_is_ready(cache_dir):
        return
    cache_dir.mkdir(parents=True, exist_ok=True)
    print(f"creating SWE-bench venv in {cache_dir}", flush=True)
    for command in venv_install_commands(cache_dir):
        print("+ " + " ".join(command), flush=True)
        result = subprocess.run(command, check=False)
        if result.returncode != 0:
            raise BenchError(f"failed to prepare the benchmark venv: {' '.join(command)}")
    if not venv_is_ready(cache_dir):
        raise BenchError("the benchmark venv was created but swebench==5.0.2 is not importable")


def reexec_in_venv(cache_dir: Path) -> None:
    """Run the rest of this script with the venv interpreter.

    The runner only needs the venv for parquet loading and for invoking the
    official harness; --help and --dry-run never get here.
    """
    target = venv_python(cache_dir)
    if os.environ.get("CODAPE_BENCH_VENV") == str(target):
        return
    # Compare sys.prefix, not sys.executable: on Linux `venv/bin/python` is a
    # symlink to the system interpreter, so resolving both sides makes them
    # look equal and skips the re-exec, leaving pyarrow unimportable.
    if Path(sys.prefix).resolve() == target.parent.parent.resolve():
        return
    env = os.environ.copy()
    env["CODAPE_BENCH_VENV"] = str(target)
    os.execve(str(target), [str(target), str(Path(__file__).resolve()), *sys.argv[1:]], env)


def download_dataset(spec: dict[str, str], dest: Path) -> None:
    if dest.exists():
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    partial = dest.with_suffix(dest.suffix + ".part")
    url = dataset_url(spec)
    print(f"downloading {url}", flush=True)
    request = urllib.request.Request(url)
    token = os.environ.get("HF_TOKEN")
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=300) as response, partial.open("wb") as handle:
            shutil.copyfileobj(response, handle)
    except (OSError, urllib.error.URLError) as exc:
        partial.unlink(missing_ok=True)
        raise BenchError(f"could not download the pinned dataset: {exc}") from exc
    partial.replace(dest)
    print(f"dataset cached at {dest}", flush=True)


def load_instances(parquet: Path) -> list[dict[str, Any]]:
    import pyarrow.parquet as pq  # imported inside the venv

    rows = pq.read_table(parquet).to_pylist()
    for row in rows:
        if "instance_id" not in row:
            raise BenchError(f"{parquet} has a row without instance_id")
    return rows


# ---------------------------------------------------------------------------
# Docker plumbing
# ---------------------------------------------------------------------------


def run_checked(
    command: list[str],
    *,
    what: str,
    capture: bool = False,
    timeout: int | None = None,
    display: str | None = None,
) -> subprocess.CompletedProcess[str]:
    print("+ " + (display or " ".join(command)), flush=True)
    result = subprocess.run(
        command,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
        timeout=timeout,
        check=False,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or "").strip()
        raise BenchError(f"{what} failed (exit {result.returncode})" + (f": {detail[-2000:]}" if detail else ""))
    return result


def ensure_image(image: str) -> None:
    inspect = subprocess.run(["docker", "image", "inspect", image], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    if inspect.returncode == 0:
        print(f"image present: {image}", flush=True)
        return
    command = ["docker", "pull"]
    platform = image_platform(image)
    if platform:
        command += ["--platform", platform]
    command.append(image)
    run_checked(command, what=f"pulling {image}")


def install_codape(container: str, binary: Path | None) -> None:
    run_checked(["docker", "exec", container, "mkdir", "-p", "/usr/local/bin"], what="preparing /usr/local/bin")
    if binary is not None:
        if not binary.exists():
            raise BenchError(f"--codape-binary does not exist: {binary}")
        if not looks_like_linux_elf(binary):
            raise BenchError(
                f"--codape-binary must be a Linux ELF binary (got {binary}). "
                "On macOS, omit the flag and let the runner install the pinned Linux release."
            )
        run_checked(["docker", "cp", str(binary), f"{container}:/usr/local/bin/codape"], what="copying Codape into the container")
        run_checked(["docker", "exec", container, "chmod", "0755", "/usr/local/bin/codape"], what="making Codape executable")
    else:
        script = INSTALL_SCRIPT.replace("__VERSION__", CODAPE_RELEASE_VERSION).replace("__TARGET__", CODAPE_TARGET)
        run_checked(
            ["docker", "exec", container, "sh", "-c", script],
            what="installing the pinned Codape release in the container",
            display=f"docker exec {container} sh -c '<install Codape {CODAPE_RELEASE_VERSION} with SHA256 verification>'",
        )
    run_checked(["docker", "exec", container, "/usr/local/bin/codape", "--version"], what="checking the container Codape binary")


def seed_config(container: str) -> None:
    settings = json.dumps({"config_version": 4, "telemetry_enabled": False})
    script = (
        f"mkdir -p {CONTAINER_CONFIG_DIR} && "
        f"printf '%s\\n' {shlex_quote(settings)} > {CONTAINER_CONFIG_DIR}/settings.json"
    )
    run_checked(["docker", "exec", container, "sh", "-c", script], what="seeding an isolated Codape config home")


def install_claude(container: str, binary: Path | None) -> None:
    binary = binary or claude_binary()
    if not binary.exists():
        raise BenchError(f"--claude-binary does not exist: {binary}")
    run_checked(
        ["docker", "cp", str(binary), f"{container}:/tmp/claude"],
        what="copying Claude Code into the container",
    )
    run_checked(["docker", "exec", container, "chmod", "0755", "/tmp/claude"], what="making Claude Code executable")
    run_checked(
        ["docker", "exec", container, "mkdir", "-p", CONTAINER_CLAUDE_CONFIG_DIR],
        what="preparing an isolated Claude Code config home",
    )


def shlex_quote(value: str) -> str:
    # Imported lazily to keep this helper obvious; shlex is stdlib.
    import shlex

    return shlex.quote(value)


def agent_command(
    container: str,
    timeout: int,
    max_turns: int,
    extra_args: list[str],
    agent: str = "codape",
    context_tokens: int = DEFAULT_CONTEXT_TOKENS,
) -> list[str]:
    command = ["docker", "exec", "-i", "-w", CONTAINER_WORKDIR]
    for name in selected_env_names():
        command += ["-e", name]
    if agent == "codape":
        command += [
            "-e",
            f"CLAUDE_CONFIG_DIR={CONTAINER_CONFIG_DIR}",
            "-e",
            "CODAPE_LOG=off",
            "-e",
            "NO_COLOR=1",
            # The instance container is the isolation boundary; the shell
            # sandbox would block the network and privilege-drop syscalls the
            # repository tests need.
            "-e",
            "CODAPE_SANDBOX=off",
            container,
            "timeout",
            "--signal=TERM",
            "--kill-after=30",
            str(timeout),
            "codape",
            "-p",
            "--dangerously-skip-permissions",
            "--no-session-persistence",
            "--no-telemetry",
            "--output-format",
            "stream-json",
            # The deliverable is the working-tree diff. Reading the issue text
            # for output paths would turn paths quoted from a reporter's build
            # log into deliverables: on fastlane__fastlane-19207 the inferred
            # set sent the run back twice and made it fabricate six files
            # under /Users and /Library.
            "--no-required-artifacts",
            # The stream is the evidence for every post-run analysis. With
            # summaries only, a transcript says a command ran but not which
            # one, so a failure cannot be attributed to a step.
            "--verbose-tool-input",
            "--max-turns",
            str(max_turns),
        ]
    else:
        # Claude Code needs the gateway settings under its own names; keep its
        # state out of the repository and stop it reaching any other host.
        # SWE-bench containers run as root, so IS_SANDBOX admits the bypass.
        command += [
            "-e",
            f"CLAUDE_CONFIG_DIR={CONTAINER_CLAUDE_CONFIG_DIR}",
            "-e",
            f"CLAUDE_CODE_MAX_CONTEXT_TOKENS={context_tokens}",
            "-e",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1",
            "-e",
            "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1",
            "-e",
            "DISABLE_AUTOUPDATER=1",
            "-e",
            "DISABLE_TELEMETRY=1",
            "-e",
            "API_TIMEOUT_MS=3000000",
            "-e",
            "IS_SANDBOX=1",
            container,
            "timeout",
            "--signal=TERM",
            "--kill-after=30",
            str(timeout),
            "/tmp/claude",
            "-p",
            "--dangerously-skip-permissions",
            "--no-session-persistence",
            "--output-format",
            "stream-json",
            "--verbose",
            "--max-turns",
            str(max_turns),
        ]
    command += extra_args
    return command


def run_agent(
    container: str,
    prompt: str,
    timeout: int,
    max_turns: int,
    extra_args: list[str],
    log_dir: Path,
    agent: str = "codape",
    context_tokens: int = DEFAULT_CONTEXT_TOKENS,
) -> dict[str, Any]:
    command = agent_command(container, timeout, max_turns, extra_args, agent, context_tokens)
    print("+ " + " ".join(command) + "   (prompt on stdin)", flush=True)
    started = time.monotonic()
    try:
        result = subprocess.run(
            command,
            input=prompt,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout + 120,
            check=False,
            env=agent_child_env(),
        )
        stdout, stderr, returncode = result.stdout, result.stderr, result.returncode
    except subprocess.TimeoutExpired as exc:
        subprocess.run(["docker", "kill", container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        stdout = _as_text(exc.stdout)
        stderr = _as_text(exc.stderr) + "\nrunner timed out while waiting for docker exec"
        returncode = None
    elapsed = time.monotonic() - started
    transcript = parse_stream_json_summary(_as_text(stdout))
    log_dir.mkdir(parents=True, exist_ok=True)
    (log_dir / "agent-stream.jsonl").write_text(stdout, encoding="utf-8")
    (log_dir / "agent-stderr.log").write_text(stderr, encoding="utf-8")
    return {
        "returncode": returncode,
        "elapsed_sec": round(elapsed, 3),
        **transcript,
        "result": result_frame(stdout),
        "stderr_tail": stderr[-2000:],
    }


def _as_text(value: bytes | str | None) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return value


def extract_patch(container: str, base_commit: str, log_dir: Path) -> str:
    if not re.fullmatch(r"[0-9a-fA-F]{7,64}", base_commit or ""):
        raise BenchError(f"refusing to diff against an unexpected base commit: {base_commit!r}")
    script = PATCH_SCRIPT.replace("__BASE__", base_commit)
    result = subprocess.run(
        ["docker", "exec", "-w", CONTAINER_WORKDIR, container, "sh", "-c", script],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    log_dir.mkdir(parents=True, exist_ok=True)
    (log_dir / "patch-extract.stderr.log").write_text(result.stderr, encoding="utf-8")
    if result.returncode != 0:
        print(f"warning: patch extraction exited {result.returncode}; using empty patch", file=sys.stderr)
        (log_dir / "patch.diff").write_text("", encoding="utf-8")
        return ""
    patch = result.stdout
    (log_dir / "patch.diff").write_text(patch, encoding="utf-8")
    return patch


def run_one_instance(
    instance: dict[str, Any],
    run_dir: Path,
    args: argparse.Namespace,
    predictions_path: Path,
    attempt: int = 1,
    write_lock: threading.Lock | None = None,
) -> dict[str, Any]:
    instance_id = instance["instance_id"]
    # An attempt gets its own directory: a repeat re-runs the same instance from
    # a fresh container, so `prompt.txt`, the transcript and the extracted patch
    # must not overwrite the earlier attempt's. With one attempt the path keeps
    # the old shape, so existing run directories still read the same way.
    instance_root = run_dir / "instances" / safe_name(instance_id)
    instance_dir = instance_root if args.n_attempts == 1 else instance_root / f"attempt-{attempt}"
    instance_dir.mkdir(parents=True, exist_ok=True)
    image = instance.get("image")
    if not image:
        raise BenchError(f"{instance_id}: dataset row has no image field")
    base_commit = str(instance.get("base_commit") or "")
    if not re.fullmatch(r"[0-9a-fA-F]{7,64}", base_commit):
        raise BenchError(f"{instance_id}: dataset row has an unexpected base_commit: {base_commit!r}")
    prompt = build_prompt(instance, args.extra_instructions)
    (instance_dir / "prompt.txt").write_text(prompt, encoding="utf-8")
    print(f"\n=== {instance_id}" + (f" (attempt {attempt} of {args.n_attempts})" if args.n_attempts > 1 else "") + " ===", flush=True)
    print(f"image: {image}", flush=True)

    # `docker pull` and the container name are both per-image/per-attempt, so
    # two threads on different instances never collide; the same instance is
    # never in flight twice.
    ensure_image(image)
    prefix = "codape" if args.agent == "codape" else "claude"
    suffix = safe_name(instance_id) if args.n_attempts == 1 else f"{safe_name(instance_id)}-a{attempt}"
    container = f"{prefix}-swebench-{args.run_id}-{suffix}"[:120]
    subprocess.run(["docker", "rm", "-f", container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    # Match the official harness: root and SYS_ADMIN, because some repository
    # test suites need an unprivileged user namespace.
    run_command = [
        "docker",
        "run",
        "-d",
        "--name",
        container,
        "--label",
        "codape-external-benchmark=1",
        "--user",
        "root",
        "--cap-add",
        "SYS_ADMIN",
    ]
    platform = image_platform(image)
    if platform:
        run_command += ["--platform", platform]
    run_command += ["-w", CONTAINER_WORKDIR, image, "sleep", "infinity"]
    started = time.monotonic()
    try:
        run_checked(run_command, what=f"starting a container for {instance_id}")
        if args.agent == "codape":
            install_codape(container, args.codape_binary)
            seed_config(container)
        else:
            install_claude(container, args.claude_binary)
        agent = run_agent(
            container,
            prompt,
            args.timeout,
            args.max_turns,
            args.codape_arg,
            instance_dir,
            agent=args.agent,
            context_tokens=args.context_tokens,
        )
        patch = extract_patch(container, base_commit, instance_dir)
    finally:
        if not args.keep_containers:
            subprocess.run(["docker", "rm", "-f", container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        else:
            print(f"kept container: {container}", flush=True)
    elapsed = round(time.monotonic() - started, 3)

    prediction = {
        "instance_id": instance_id,
        "model_name_or_path": args.model_name,
        "model_patch": patch,
    }
    # Instances run concurrently, and a JSONL line has to be whole: two threads
    # appending to the same handle would otherwise interleave. The lock is
    # per-run, so a single-threaded run takes it uncontended.
    line = json.dumps(prediction) + "\n"
    if write_lock is None:
        handle = predictions_path.open("a", encoding="utf-8")
        try:
            handle.write(line)
            handle.flush()
            os.fsync(handle.fileno())
        finally:
            handle.close()
    else:
        with write_lock, predictions_path.open("a", encoding="utf-8") as handle:
            handle.write(line)
            handle.flush()
            os.fsync(handle.fileno())

    record = {
        "instance_id": instance_id,
        "attempt": attempt,
        "agent": args.agent,
        "image": image,
        "language": language_for(instance, args.dataset),
        "log_parser": instance.get("log_parser"),
        "patch_bytes": len(patch.encode("utf-8")),
        "elapsed_sec": elapsed,
        "agent_returncode": agent["returncode"],
        "agent_elapsed_sec": agent["elapsed_sec"],
        "agent_failed": bool(agent["result"] and agent["result"].get("is_error")) or agent["returncode"] not in (0, 124),
        "agent_result_subtype": (agent["result"] or {}).get("subtype"),
        "usage": agent["usage"],
        "result_frames": agent["result_frames"],
        "agent_duration_ms": agent["duration_ms"],
        "num_turns": agent["num_turns"],
        "model_ms": agent["model_ms"],
        "tool_ms": agent["tool_ms"],
    }
    (instance_dir / "record.json").write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    print(
        f"{instance_id}{'' if args.n_attempts == 1 else f' (attempt {attempt})'}: "
        f"patch={record['patch_bytes']} bytes agent_exit={record['agent_returncode']} "
        f"elapsed={record['elapsed_sec']}s",
        flush=True,
    )
    return record


def attempt_predictions_path(run_dir: Path, args: argparse.Namespace, attempt: int) -> Path:
    """Where one attempt's predictions go.

    The official harness keys a predictions file by `instance_id`, so N
    attempts cannot share one: the second would overwrite the first and the
    evaluation would see one trial. Each attempt therefore gets its own file
    and is graded on its own; `summary.json` adds the reports up.
    """
    if args.predictions:
        base = Path(args.predictions).expanduser().resolve()
    else:
        base = run_dir / "predictions.jsonl"
    if args.n_attempts == 1:
        return base
    return base.with_name(f"{base.stem}.attempt-{attempt}{base.suffix}")


def run_attempts(
    selected: list[dict[str, Any]],
    run_dir: Path,
    args: argparse.Namespace,
    predictions_paths: dict[int, Path],
) -> list[dict[str, Any]]:
    """Run every (instance, attempt) pair, `--n-concurrent` at a time.

    Each instance is independent — its own container, its own image, its own
    conversation — so the only shared state is the predictions file (written
    under a lock) and the console. A failure in one attempt raises as it
    would sequentially; the instances already running are allowed to finish
    and their predictions stay on disk, which is what makes `--resume` useful
    after a provider outage rather than after nothing.
    """
    jobs = [(row, attempt) for attempt in sorted(predictions_paths) for row in selected]
    if not jobs:
        return []
    lock = threading.Lock() if args.n_concurrent > 1 else None
    records: list[dict[str, Any]] = []
    if args.n_concurrent == 1:
        for row, attempt in jobs:
            records.append(
                run_one_instance(row, run_dir, args, predictions_paths[attempt], attempt, lock)
            )
        return records

    print(
        f"generation: {len(jobs)} trial(s) over {len(selected)} instance(s), "
        f"{args.n_concurrent} at a time",
        flush=True,
    )
    pending = iter(jobs)
    in_flight: dict[Any, tuple[dict[str, Any], int]] = {}
    with ThreadPoolExecutor(max_workers=args.n_concurrent) as pool:
        def submit_next() -> bool:
            try:
                row, attempt = next(pending)
            except StopIteration:
                return False
            future = pool.submit(
                run_one_instance, row, run_dir, args, predictions_paths[attempt], attempt, lock
            )
            in_flight[future] = (row, attempt)
            return True

        for _ in range(args.n_concurrent):
            if not submit_next():
                break
        try:
            while in_flight:
                done, _ = wait(list(in_flight), return_when=FIRST_COMPLETED)
                for future in done:
                    in_flight.pop(future)
                    records.append(future.result())
                    submit_next()
        except BaseException:
            # A failed attempt must not leave its siblings' containers behind:
            # the per-call `finally` only runs for calls that started.
            pool.shutdown(wait=True, cancel_futures=True)
            raise

    # Records are appended as attempts finish, so the summary lists them in
    # completion order. Sort so `summary.json` reads the same as a sequential
    # run of the same jobs.
    order = {row["instance_id"]: index for index, row in enumerate(selected)}
    records.sort(key=lambda record: (order.get(record["instance_id"], 0), record.get("attempt") or 1))
    return records


def build_eval_command(
    parquet: Path,
    predictions: Path,
    run_id: str,
    report_dir: Path,
    instance_ids: Iterable[str],
    max_workers: int,
    timeout: int,
    python: str | None = None,
) -> list[str]:
    command = [
        python or sys.executable,
        "-m",
        "swebench.harness.run_evaluation",
        "--dataset_name",
        str(parquet),
        "--split",
        "test",
        "--predictions_path",
        str(predictions),
        "--run_id",
        run_id,
        "--max_workers",
        str(max_workers),
        "--timeout",
        str(timeout),
        "--report_dir",
        str(report_dir),
        "--instance_ids",
        *list(instance_ids),
    ]
    return command


def run_evaluation(eval_dir: Path, report_dir: Path, command: list[str], log_path: Path) -> int:
    eval_dir.mkdir(parents=True, exist_ok=True)
    report_dir.mkdir(parents=True, exist_ok=True)
    print("\n=== official SWE-bench evaluation ===", flush=True)
    print("+ " + " ".join(command), flush=True)
    started = time.monotonic()
    with log_path.open("w", encoding="utf-8") as log:
        process = subprocess.Popen(
            command,
            cwd=str(eval_dir),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        assert process.stdout is not None
        for line in process.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
            log.write(line)
        returncode = process.wait()
    print(f"evaluation exited {returncode} after {time.monotonic() - started:.1f}s", flush=True)
    return returncode


def find_report(report_dir: Path, model_name: str, run_id: str) -> Path | None:
    expected = report_dir / f"{model_name.replace('/', '__')}.{run_id}.json"
    if expected.exists():
        return expected
    candidates = sorted(report_dir.glob(f"*{run_id}*.json"))
    return candidates[-1] if candidates else None


def summarize(
    args: argparse.Namespace,
    run_dir: Path,
    records: list[dict[str, Any]],
    report_path: Path | None,
    timings: dict[str, Any],
    predictions_path: Path,
    selected: list[dict[str, Any]],
    attempt_reports: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """Turn the run's artifacts into `summary.json`.

    With `--n-attempts N` there are N reports, one per attempt, each over the
    same instances. pass@1 is resolved trials over total trials — the sum of
    the reports' numerators over the sum of their denominators — which is why
    the single-attempt case needs no special case: it sums one report.
    """
    def load(path: str | None) -> dict[str, Any] | None:
        if not path:
            return None
        try:
            return json.loads(Path(path).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return None

    reports = [load(entry.get("report")) for entry in (attempt_reports or [])]
    reports = [report for report in reports if report]
    if not reports and report_path is not None:
        single = load(str(report_path))
        reports = [single] if single else []
    total = None
    resolved = None
    pass_at_1 = None
    if reports:
        # Counted over trials, not over instances: with two attempts, an
        # instance resolved once is 1 of 2, and reporting "1/1 resolved" beside
        # a pass@1 of 0.5 would read as a contradiction. `total` is the sum of
        # the per-attempt instance counts, so pass@1 = resolved / total and the
        # two numbers in the summary agree.
        per_attempt = [
            (report.get("resolved_instances"), report.get("total_instances"))
            for report in reports
        ]
        if all(r is not None and t for r, t in per_attempt):
            resolved = sum(r for r, _ in per_attempt)
            total = sum(t for _, t in per_attempt)
            pass_at_1 = resolved / total
        else:
            ratios = [
                report["resolved_instances"] / report["total_instances"]
                for report in reports
                if report.get("total_instances") and report.get("resolved_instances") is not None
            ]
            pass_at_1 = sum(ratios) / len(ratios) if ratios else None
            if len(reports) == 1:
                total = reports[0].get("total_instances")
                resolved = reports[0].get("resolved_instances")

    usages = [record["usage"] for record in records if record.get("usage")]
    token_usage = None
    if usages:
        token_usage = {
            key: sum((usage.get(key) or 0) for usage in usages)
            for key in ("input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")
        }
        token_usage["total_cost_usd"] = sum((usage.get("total_cost_usd") or 0.0) for usage in usages)

    # Where the time went, summed over the instances whose binary recorded it.
    # `recorded_instances` is how many that was, so a partial sum cannot read
    # as a whole-run figure.
    timed = [record for record in records if record.get("model_ms") is not None]
    # The part of the run neither number covers: context building, hooks,
    # compaction and the gaps between turns. It is not a remainder to be
    # waved away — measured against the static provider, a two-turn tool run
    # spent ~41 ms here against 14 ms waiting on the provider, so reporting
    # only `model_ms` and `tool_ms` invites the reader to conclude the tools
    # dominate when neither explains half the run. Summed only over records
    # that carry all three, so a partially recorded instance cannot make the
    # three disagree with the clock they came from.
    accounted = [
        record
        for record in timed
        if record.get("tool_ms") is not None and record.get("agent_duration_ms") is not None
    ]

    def _unattributed() -> int | None:
        if not accounted:
            return None
        total = 0
        for record in accounted:
            turn = record["agent_duration_ms"]
            spent = (record["model_ms"] or 0) + (record["tool_ms"] or 0)
            # Saturating: more attributable time than wall clock would be a
            # measurement bug, and a negative remainder would hide it inside
            # a number that reads as a plausible figure.
            total += max(turn - spent, 0)
        return total

    timing = {
        "model_ms": sum(record.get("model_ms") or 0 for record in timed) if timed else None,
        "tool_ms": sum(record.get("tool_ms") or 0 for record in timed) if timed else None,
        "agent_duration_ms": sum(record.get("agent_duration_ms") or 0 for record in timed) if timed else None,
        "unattributed_ms": _unattributed(),
        "num_turns": sum(record.get("num_turns") or 0 for record in timed) if timed else None,
        "recorded_instances": len(timed),
    }

    summary = {
        "benchmark": "swebench",
        "dataset": args.dataset,
        "dataset_revision": args._dataset["revision"],
        "dataset_repo": args._dataset["repo"],
        "languages": sorted({language_for(row, args.dataset) for row in selected}),
        "split": "test",
        "run_id": args.run_id,
        "model_name_or_path": args.model_name,
        "instances": [record["instance_id"] for record in records],
        "attempts": max((record.get("attempt") or 1) for record in records) if records else 1,
        "resolved_instances": resolved,
        "total_instances": total,
        "pass_at_1": pass_at_1,
        "generation": {
            "elapsed_sec": timings.get("generation_elapsed_sec"),
            "instance_records": records,
        },
        "evaluation": {
            "elapsed_sec": timings.get("evaluation_elapsed_sec"),
            "returncode": timings.get("evaluation_returncode"),
            "report": str(report_path) if report_path else None,
            "attempts": [
                {
                    "attempt": entry.get("attempt"),
                    "predictions": entry.get("path"),
                    "report": entry.get("report"),
                    "returncode": entry.get("returncode"),
                }
                for entry in (attempt_reports or [])
            ],
        },
        "total_elapsed_sec": timings.get("total_elapsed_sec"),
        "token_usage": token_usage,
        "timing": timing,
        "predictions": str(predictions_path),
        "notes": [
            "pass@1 is resolved trials / total trials over the official reports; with --n-attempts N that is the sum of N reports, and with one attempt it is the report's own counts.",
            "token_usage and timing are summed over the instances whose result frames carried them; null means the binary did not report it.",
            "timing.unattributed_ms is agent_duration_ms minus model_ms and tool_ms, summed over instances that recorded all three: context building, hooks, compaction and inter-turn gaps. Against a local provider it dominated, because model time was milliseconds; against a real provider, read it before concluding the tools dominate.",
        ],
    }
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    return summary


def print_summary(summary: dict[str, Any]) -> None:
    print("\n=== SWE-bench summary ===")
    print(f"dataset     : {summary['dataset_repo']} @ {summary['dataset_revision']}")
    print(f"run id      : {summary['run_id']}")
    print(f"model       : {summary['model_name_or_path']}")
    if summary.get("languages"):
        print(f"languages   : {', '.join(summary['languages'])}")
    print(f"instances   : {', '.join(dict.fromkeys(summary['instances'])) or '(none)'}")
    # With `--n-attempts` the denominator is trials, not distinct instances, and
    # a line that called it "instances" would look like a count of the list
    # printed above it.
    unit = "trials" if (summary.get("attempts") or 1) > 1 else "instances"
    if summary["pass_at_1"] is not None:
        print(
            f"pass@1      : {summary['pass_at_1']:.4f} "
            f"({summary['resolved_instances']}/{summary['total_instances']} {unit} resolved)"
        )
    else:
        print("pass@1      : unavailable (official report missing or incomplete)")
    if summary["generation"]["elapsed_sec"] is not None:
        print(f"generation  : {summary['generation']['elapsed_sec']:.1f}s")
    if summary["evaluation"]["elapsed_sec"] is not None:
        print(f"evaluation  : {summary['evaluation']['elapsed_sec']:.1f}s")
    print(f"total       : {summary['total_elapsed_sec']:.1f}s")
    if summary["token_usage"] is None:
        print("tokens/cost : unavailable (no usage in the run's result frames)")
    else:
        print(f"tokens/cost : {json.dumps(summary['token_usage'])}")
    timing = summary.get("timing") or {}
    if timing.get("model_ms") is not None:
        unattributed = timing.get("unattributed_ms")
        # Printed beside the two attributable figures, because on its own
        # "model=14s tools=33s" reads as the whole story when more than half
        # the run was neither.
        print(
            f"time        : model={timing['model_ms'] / 1000:.1f}s "
            f"tools={timing['tool_ms'] / 1000:.1f}s "
            + (
                f"other={unattributed / 1000:.1f}s "
                if unattributed is not None
                else ""
            )
            + f"turns={timing['num_turns'] or 0} "
            f"({timing['recorded_instances']} of {summary['total_instances']} {unit} recorded)"
        )
    else:
        print("time        : unavailable (no model_ms/tool_ms in the run's result frames)")
    print(f"predictions : {summary['predictions']}")
    if summary["evaluation"]["report"]:
        print(f"report      : {summary['evaluation']['report']}")
    print(
        "SWEBENCH_SUMMARY "
        f"run_id={summary['run_id']} "
        f"pass_at_1={summary['pass_at_1'] if summary['pass_at_1'] is not None else 'na'} "
        f"resolved={summary['resolved_instances'] if summary['resolved_instances'] is not None else 'na'} "
        f"total={summary['total_instances'] if summary['total_instances'] is not None else 'na'} "
        f"elapsed_sec={summary['total_elapsed_sec']:.1f} "
        f"tokens={'na' if summary['token_usage'] is None else 'present'} "
        f"model_sec={(timing['model_ms'] / 1000) if timing.get('model_ms') is not None else 'na'} "
        f"tool_sec={(timing['tool_ms'] / 1000) if timing.get('tool_ms') is not None else 'na'} "
        f"other_sec={(timing['unattributed_ms'] / 1000) if timing.get('unattributed_ms') is not None else 'na'}"
    )


# ---------------------------------------------------------------------------
# Dry run
# ---------------------------------------------------------------------------


def dry_run(args: argparse.Namespace, spec: dict[str, str], cache_dir: Path) -> None:
    parquet = dataset_cache_path(cache_dir, spec)
    predictions = Path(args.predictions).expanduser().resolve() if args.predictions else (args.output_dir / args.run_id / "predictions.jsonl")
    print("SWE-bench dry run; nothing will be downloaded or executed.")
    print(f"pinned package : swebench=={SWEBENCH_VERSION}")
    print(f"dataset        : {spec['repo']} @ {spec['revision']} ({spec['alias']})")
    if spec["alias"] == "multilingual":
        print(f"languages      : {', '.join(MULTILINGUAL_LANGUAGES)}")
    print(f"dataset URL    : {dataset_url(spec)}")
    print(f"dataset cache  : {parquet} ({'cached' if parquet.exists() else 'not cached'})")
    print(f"venv cache     : {cache_dir / f'venv-swebench-{SWEBENCH_VERSION}'}")
    print(f"run directory  : {args.output_dir / args.run_id}")
    print(f"predictions    : {predictions}")
    print()

    ready, message = docker_available()
    if ready:
        print(f"docker         : {message}")
    else:
        print(f"docker         : NOT AVAILABLE ({message}) -- dry run continues; a real run needs Docker")
    env_names = selected_env_names()
    print("model env      : " + (", ".join(env_names) if env_names else "none set (a real run needs an API key)"))
    print(f"agent          : {args.agent}")
    print(f"context window : {args.context_tokens} tokens")
    if args.agent == "codape":
        if args.codape_binary:
            print(f"codape binary : {args.codape_binary}")
        else:
            print(f"codape binary : download {CODAPE_RELEASE_VERSION} ({CODAPE_TARGET}) inside each container")
    else:
        try:
            binary = args.claude_binary or claude_binary()
        except BenchError:
            print("claude binary : NOT FOUND on PATH (a real run needs Claude Code)")
        else:
            print(f"claude binary : {binary}")
    print()

    print("# 1. Prepare the isolated evaluation venv (only for a real run)")
    for command in venv_install_commands(cache_dir):
        print("+ " + " ".join(command))
    print()

    print("# 2. Download the pinned dataset (once; cached by revision)")
    print(f"+ curl -fL {dataset_url(spec)} -o {parquet}")
    print()

    if args.eval_only:
        prediction_path = Path(args.eval_only).expanduser().resolve()
        predictions = prediction_path
        print("# 3. Generation skipped: --eval-only")
        print(f"# predictions: {prediction_path}")
        instance_ids = list(read_predictions(prediction_path))
        attempt_paths = {0: prediction_path}
    else:
        if args.instances:
            instance_ids = list(args.instances)
            print("# 3. Generation for the explicitly named instances")
        else:
            limit = 1 if args.limit is None else args.limit
            instance_ids = ["<selected-instance-ids>"]
            print(f"# 3. Generation for the first {limit} instance(s), sorted by instance_id")
        attempt_paths = {
            attempt: attempt_predictions_path(args.output_dir / args.run_id, args, attempt)
            for attempt in range(1, args.n_attempts + 1)
        }
        predictions = attempt_paths[1]
        if args.n_concurrent > 1:
            print(f"#    {args.n_concurrent} instance(s) in flight at a time")
        if args.n_attempts > 1:
            print(
                f"#    {args.n_attempts} attempts per instance, each with its own "
                "predictions file and evaluation:"
            )
            for attempt, path in sorted(attempt_paths.items()):
                print(f"#      attempt {attempt}: {path}")
        prefix = "codape" if args.agent == "codape" else "claude"
        container = f"{prefix}-swebench-{args.run_id}-<instance>"
        if args.n_attempts > 1:
            container += "-a<attempt>"
        print(f"+ docker pull --platform linux/amd64 <image from dataset row>")
        print(f"+ docker run -d --name {container} --user root --cap-add SYS_ADMIN --platform linux/amd64 -w {CONTAINER_WORKDIR} <image> sleep infinity")
        if args.agent == "codape":
            if args.codape_binary:
                print(f"+ docker cp {args.codape_binary} {container}:/usr/local/bin/codape")
                print(f"+ docker exec {container} chmod 0755 /usr/local/bin/codape")
            else:
                print(f"+ docker exec {container} sh -c '<pinned Codape {CODAPE_RELEASE_VERSION} download + SHA256SUMS verification>'")
            print(f"+ docker exec {container} sh -c '<seed {CONTAINER_CONFIG_DIR}/settings.json with telemetry_enabled=false>'")
        else:
            try:
                binary = args.claude_binary or claude_binary()
            except BenchError:
                binary = Path("claude")
            print(f"+ docker cp {binary} {container}:/tmp/claude")
            print(f"+ docker exec {container} chmod 0755 /tmp/claude")
            print(f"+ docker exec {container} mkdir -p {CONTAINER_CLAUDE_CONFIG_DIR}")
        agent = agent_command(container, args.timeout, args.max_turns, args.codape_arg, args.agent, args.context_tokens)
        print("+ " + " ".join(agent) + "   (issue text on stdin)")
        print(f"+ docker exec -w {CONTAINER_WORKDIR} {container} sh -c '<git reset -q INSTANCE_BASE_COMMIT; git -c core.fileMode=false add -A; git diff --cached --binary INSTANCE_BASE_COMMIT>'")
        print(f"+ write one JSONL line to {predictions}: instance_id, model_name_or_path={args.model_name}, model_patch")
        if not args.keep_containers:
            print(f"+ docker rm -f {container}")
        print()

    print("# 4. Official evaluation harness")
    for attempt, path in sorted(attempt_paths.items()):
        label = args.run_id if len(attempt_paths) == 1 else f"{args.run_id}-a{attempt}"
        reports = args.output_dir / args.run_id / "eval" / "reports"
        if len(attempt_paths) > 1:
            reports = reports / f"attempt-{attempt}"
        eval_command = build_eval_command(
            parquet,
            path,
            label,
            reports,
            instance_ids,
            args.max_workers,
            args.eval_timeout,
            python=str(venv_python(cache_dir)),
        )
        if len(attempt_paths) > 1:
            print(f"# attempt {attempt}")
        print("+ " + " ".join(eval_command))
    print(f"# cwd: {args.output_dir / args.run_id / 'eval'}")
    print()
    print("# 5. Report pass@1 from the official report JSON and write summary.json")
    print("SWE-bench dry run complete.")


# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------


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


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run Codape (or Claude Code) on a reproducible SWE-bench subset and grade with the official harness.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  # Inspect every command without Docker, keys, or downloads
  scripts/external-benchmarks/swebench/run.sh --dry-run --limit 1

  # One Verified instance end to end
  export ANTHROPIC_API_KEY=... ANTHROPIC_BASE_URL=... ANTHROPIC_MODEL=...
  scripts/external-benchmarks/swebench/run.sh --limit 1 --instance astropy__astropy-12907

  # The same instance under Claude Code, with the same model and budget
  scripts/external-benchmarks/swebench/run.sh --agent claude-code \
      --limit 1 --instance astropy__astropy-12907 --run-id astropy-claude

  # Re-grade an existing predictions file without spending model tokens
  scripts/external-benchmarks/swebench/run.sh --eval-only target/swe-bench/preds.jsonl

Notes:
  * The subset is sorted by instance_id, so --limit N is reproducible.
  * Codape is installed inside each official image from a pinned release; use
    --codape-binary to test a locally built Linux ELF binary instead.
  * --agent claude-code is the harness control: hold the model, image, budget,
    and context window equal, and change only the agent.  Set the gateway
    header in ANTHROPIC_CUSTOM_HEADERS (or CODAPE_CUSTOM_HEADERS; the runner
    normalizes it) so both arms send the same request metadata.
  * API keys are read from the environment and passed to docker by variable
    name only. They are never written to a repository file.
""",
    )
    parser.add_argument(
        "--dataset",
        choices=sorted(DATASETS),
        default="verified",
        help="Dataset alias: verified (500), lite (300), or multilingual (300; default: verified)",
    )
    parser.add_argument("--dataset-revision", help="Override the pinned Hugging Face revision (advanced)")
    parser.add_argument("--instance", dest="instances", action="append", metavar="ID", help="Instance ID; repeatable")
    parser.add_argument("--limit", type=positive_int, help="Number of instances from the sorted dataset (default: 1)")
    parser.add_argument("--timeout", type=positive_int, default=DEFAULT_AGENT_TIMEOUT, help=f"Per-instance agent timeout in seconds (default: {DEFAULT_AGENT_TIMEOUT})")
    parser.add_argument("--eval-timeout", type=positive_int, default=DEFAULT_EVAL_TIMEOUT, help=f"Per-instance test timeout for the official harness (default: {DEFAULT_EVAL_TIMEOUT})")
    parser.add_argument("--max-turns", type=positive_int, default=DEFAULT_MAX_TURNS, help=f"Codape --max-turns (default: {DEFAULT_MAX_TURNS})")
    parser.add_argument("--predictions", help="Predictions JSONL path (default: <run-dir>/predictions.jsonl)")
    parser.add_argument("--eval-only", metavar="PATH", help="Skip generation and grade this predictions JSONL with the official harness")
    parser.add_argument("--agent", choices=("codape", "claude-code"), default="codape", help="Agent harness to run inside the instance container (default: codape)")
    parser.add_argument("--model-name", help="model_name_or_path written to predictions (default: codape-<model>)")
    parser.add_argument("--codape-binary", type=Path, help="Linux x86_64 ELF Codape binary to copy into containers")
    parser.add_argument("--claude-binary", type=Path, help="Claude Code executable to copy into containers (default: claude on PATH)")
    parser.add_argument("--context-tokens", type=positive_int, default=DEFAULT_CONTEXT_TOKENS, help=f"Context window both harnesses are told about (default: {DEFAULT_CONTEXT_TOKENS})")
    parser.add_argument("--codape-arg", action="append", default=[], metavar="ARG", help="Extra argument passed to Codape; repeatable")
    parser.add_argument("--extra-instructions", help="Extra text appended to every issue prompt")
    parser.add_argument("--run-id", help="Run identifier (default: swebench-YYYYmmdd-HHMMSS)")
    parser.add_argument("--output-dir", type=Path, default=REPO_ROOT / "target" / "external-benchmarks" / "swebench", help="Root for run artifacts (default: target/external-benchmarks/swebench)")
    parser.add_argument("--cache-dir", type=Path, help="Cache root for the venv and dataset (default: $CODAPE_BENCH_CACHE or ~/.cache/codape-external-benchmarks)")
    parser.add_argument("--max-workers", type=positive_int, default=1, help="Official harness evaluation workers (default: 1)")
    parser.add_argument(
        "--n-concurrent",
        type=positive_int,
        default=DEFAULT_N_CONCURRENT,
        help="Instances generated at once (default: %(default)s). Instances are "
        "independent, so this is pure throughput; each one holds a repository "
        "image and a model stream, so raise it as far as RAM and the provider "
        "allow",
    )
    parser.add_argument(
        "--n-attempts",
        type=positive_int,
        default=DEFAULT_N_ATTEMPTS,
        help="Attempts per instance (default: %(default)s). Repeats are what "
        "separate a stable result from a lucky one; pass@1 stays resolved / "
        "total over every attempt, which is the harness definition",
    )
    parser.add_argument("--keep-containers", action="store_true", help="Leave per-instance agent containers running for debugging")
    parser.add_argument("--resume", action="store_true", help="Reuse existing predictions and skip completed instance IDs")
    parser.add_argument("--dry-run", action="store_true", help="Print commands without downloading, installing, or running anything")
    args = parser.parse_args(argv)

    if args.instances and args.limit is not None:
        parser.error("use --instance or --limit, not both")
    if args.eval_only and args.instances:
        parser.error("--eval-only takes its instance IDs from the predictions file; do not pass --instance")
    if args.eval_only and args.limit is not None:
        parser.error("--eval-only takes its instance size from the predictions file; do not pass --limit")
    if args.eval_only and not Path(args.eval_only).expanduser().exists():
        parser.error(f"--eval-only file does not exist: {args.eval_only}")
    if args.codape_binary and not args.codape_binary.exists():
        parser.error(f"--codape-binary does not exist: {args.codape_binary}")
    if args.codape_binary and not looks_like_linux_elf(args.codape_binary):
        parser.error(f"--codape-binary must be a Linux ELF binary: {args.codape_binary}")
    if args.claude_binary and not args.claude_binary.expanduser().is_file():
        parser.error(f"--claude-binary does not exist: {args.claude_binary}")
    if args.agent == "claude-code" and args.codape_binary:
        parser.error("--codape-binary is only used with --agent codape")
    if args.agent == "codape" and args.claude_binary:
        parser.error("--claude-binary is only used with --agent claude-code")
    if args.run_id is None:
        args.run_id = f"swebench-{utc_stamp()}"
    if not re.fullmatch(r"[A-Za-z0-9._-]+", args.run_id):
        parser.error("--run-id may contain only letters, digits, dot, underscore, and hyphen")
    args.output_dir = args.output_dir.expanduser().resolve()
    if args.claude_binary:
        args.claude_binary = args.claude_binary.expanduser().resolve()
    args.cache_dir = resolve_cache_dir(str(args.cache_dir) if args.cache_dir else None)
    args._dataset = dataset_spec(args.dataset, args.dataset_revision)
    args.model_name = prediction_model_name(args.model_name)
    return args


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    spec = args._dataset
    cache_dir = args.cache_dir

    if args.dry_run:
        dry_run(args, spec, cache_dir)
        return 0

    try:
        ready, message = docker_available()
        if not ready:
            raise BenchError(f"Docker is required for a real run: {message}")
        if not args.eval_only:
            require_api_key()

        ensure_venv(cache_dir)
        reexec_in_venv(cache_dir)

        parquet = dataset_cache_path(cache_dir, spec)
        download_dataset(spec, parquet)
        rows = load_instances(parquet)

        run_dir = args.output_dir / args.run_id
        run_dir.mkdir(parents=True, exist_ok=True)
        predictions_path = Path(args.predictions).expanduser().resolve() if args.predictions else run_dir / "predictions.jsonl"
        predictions_path.parent.mkdir(parents=True, exist_ok=True)

        if args.eval_only:
            predictions_path = Path(args.eval_only).expanduser().resolve()
            existing = read_predictions(predictions_path)
            instance_ids = list(existing)
            selected = select_instances(rows, instance_ids, None)
            validate_instances(selected, args.dataset)
            all_selected = selected
            generation_records: list[dict[str, Any]] = []
            generation_elapsed = None
            attempt_paths = {}
        else:
            selected = select_instances(rows, args.instances, args.limit)
            validate_instances(selected, args.dataset)
            all_selected = selected
            instance_ids = [row["instance_id"] for row in selected]
            attempt_paths = {
                attempt: attempt_predictions_path(run_dir, args, attempt)
                for attempt in range(1, args.n_attempts + 1)
            }
            if args.resume:
                # Resume is per attempt: a repeat is a separate trial, so the
                # second attempt is not satisfied by the first one's
                # prediction. Instance IDs are the same set in every file.
                remaining = []
                for row in selected:
                    if all(
                        path.exists() and row["instance_id"] in read_predictions(path)
                        for path in attempt_paths.values()
                        if path.exists()
                    ) and all(path.exists() for path in attempt_paths.values()):
                        continue
                    remaining.append(row)
                done = len(selected) - len(remaining)
                selected = remaining
                print(
                    f"resuming: {done} instance(s) have every attempt, "
                    f"{len(selected)} to run",
                    flush=True,
                )
            else:
                for path in attempt_paths.values():
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text("", encoding="utf-8")
            print(
                f"pinned dataset: {spec['repo']} @ {spec['revision']} ({len(rows)} instances)",
                flush=True,
            )
            print(f"selected {len(selected)} instance(s): {', '.join(row['instance_id'] for row in selected)}", flush=True)
            generation_started = time.monotonic()
            generation_records = run_attempts(selected, run_dir, args, attempt_paths)
            generation_elapsed = time.monotonic() - generation_started

        # Grade each attempt's predictions separately: the official harness keys
        # a file by instance_id, so a single file can hold one trial per
        # instance. `summarize` then adds the reports up as trials, which is the
        # same quantity one attempt graded once produces.
        eval_dir = run_dir / "eval"
        report_dir = eval_dir / "reports"
        if not attempt_paths:
            attempt_paths = {0: Path(args.eval_only).expanduser().resolve()}
        eval_started = time.monotonic()
        attempt_reports: list[dict[str, Any]] = []
        returncodes: list[int] = []
        predictions: dict[str, dict[str, Any]] = {}
        for index, (attempt, path) in enumerate(sorted(attempt_paths.items())):
            attempt_predictions = read_predictions(path) if path.exists() else {}
            predictions.update(attempt_predictions)
            ids = [iid for iid in instance_ids if iid in attempt_predictions]
            if not ids:
                if args.eval_only:
                    raise BenchError(f"no predictions found in {path}")
                print(f"warning: {path} has no predictions for the selected instances", file=sys.stderr)
                attempt_reports.append({"attempt": attempt, "path": str(path), "report": None, "returncode": None})
                continue
            # A repeat's evaluation must not share the harness's run_id, or the
            # second attempt's report overwrites the first.
            label = args.run_id if len(attempt_paths) == 1 else f"{args.run_id}-a{attempt}"
            sub_reports = report_dir if len(attempt_paths) == 1 else report_dir / f"attempt-{attempt}"
            command = build_eval_command(parquet, path, label, sub_reports, ids, args.max_workers, args.eval_timeout)
            log_path = eval_dir / ("evaluation.log" if len(attempt_paths) == 1 else f"evaluation-attempt-{attempt}.log")
            returncode = run_evaluation(eval_dir, sub_reports, command, log_path)
            returncodes.append(returncode)
            attempt_reports.append(
                {
                    "attempt": attempt,
                    "path": str(path),
                    "report": str(found) if (found := find_report(sub_reports, args.model_name, label)) else None,
                    "returncode": returncode,
                }
            )
        eval_elapsed = time.monotonic() - eval_started
        returncode = max(returncodes) if returncodes else 0
        report_path = (
            Path(attempt_reports[0]["report"])
            if len(attempt_reports) == 1 and attempt_reports[0]["report"]
            else None
        )
        if not predictions:
            raise BenchError(f"no predictions found in {', '.join(str(p) for p in attempt_paths.values())}")

        if not generation_records:
            generation_records = [
                {
                    "instance_id": iid,
                    "image": next((row.get("image") for row in rows if row["instance_id"] == iid), None),
                    "patch_bytes": len((predictions[iid].get("model_patch") or "").encode("utf-8")),
                    "elapsed_sec": None,
                    "agent_returncode": None,
                    "agent_elapsed_sec": None,
                    "agent_failed": None,
                    "agent_result_subtype": None,
                    "usage": None,
                }
                for iid in instance_ids
            ]

        timings = {
            "generation_elapsed_sec": generation_elapsed,
            "evaluation_elapsed_sec": eval_elapsed,
            "evaluation_returncode": returncode,
            "total_elapsed_sec": (generation_elapsed or 0.0) + eval_elapsed,
        }
        summary = summarize(
            args,
            run_dir,
            generation_records,
            report_path,
            timings,
            next(iter(attempt_paths.values())),
            all_selected,
            attempt_reports=attempt_reports,
        )
        print_summary(summary)
        if returncode != 0:
            print("warning: the official harness exited non-zero; inspect evaluation.log", file=sys.stderr)
            return 1
        return 0
    except BenchError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
