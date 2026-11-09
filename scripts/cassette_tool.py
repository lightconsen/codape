#!/usr/bin/env python3
"""Maintain the committed LLM cassettes.

Subcommands:
  check [--max-age-days N] [--max-mb N] [--strict]
  backfill [--no-git-date]
  rekey --template PATH [--dry-run] [SUITE ...]
  prune [--dry-run]

`check` never modifies files. It reports malformed/missing-marker files,
recordings without metadata, stale recordings and suites over the size budget.
`backfill` fills metadata for recordings made before metadata existed.
`rekey` rewrites the tool definitions in every recorded request from a
current traced request and renames each cassette to its new key. A tool
schema lives in the request body, so adding a parameter changes the key the
recorder computes and otherwise forces a re-record against a real endpoint.
`prune` replays each suite with tracing and deletes cassettes the suite no
longer requests.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CASSETTES = ROOT / "crates/cli/tests/cassettes"

# suite -> [(test binary, optional name filter)]
SUITES: dict[str, list[tuple[str, str | None]]] = {
    "live": [
        ("live_basic_chat", None),
        ("live_bash_tool", None),
        ("live_file_read", None),
    ],
    "e2e_code_agent": [("e2e_code_agent", None)],
    "e2e_equivalence": [("e2e_equivalence", None)],
    "benchmark_live": [("code_agent_benchmark", None)],
    "interactive_tui_live": [("interactive_tui", "interactive_live")],
}

# The digest the recorder writes into metadata: lowercase SHA-256 hex.
DIGEST_PATTERN = re.compile(r"[0-9a-f]{64}")

SECRET_PATTERNS = [
    re.compile(r"sk-ant-[A-Za-z0-9_\-]{10,}"),
    re.compile(r"sk-[A-Za-z0-9]{20,}"),
    re.compile(r"(?i)authorization\s*[:=]\s*bearer\s+[A-Za-z0-9._\-]{20,}"),
    re.compile(r"(?i)x-api-key\s*[:=]\s*[A-Za-z0-9._\-]{20,}"),
]


def cassette_files() -> list[Path]:
    if not CASSETTES.is_dir():
        return []
    return sorted(p for p in CASSETTES.rglob("*.json") if p.is_file())


def parse_time(value: object) -> dt.datetime | None:
    if not isinstance(value, str) or not value.strip():
        return None
    try:
        return dt.datetime.fromisoformat(value.strip().replace("Z", "+00:00"))
    except ValueError:
        return None


def human_size(size: int) -> str:
    return f"{size / (1024 * 1024):.1f} MiB"


def metadata_of(document: dict) -> dict:
    metadata = document.get("metadata")
    return metadata if isinstance(metadata, dict) else {}


def check(args: argparse.Namespace) -> int:
    max_age = args.max_age_days
    max_bytes = args.max_mb * 1024 * 1024
    now = dt.datetime.now(dt.timezone.utc)
    files = cassette_files()
    errors: list[str] = []
    warnings: list[str] = []
    missing_metadata: list[Path] = []
    stale: list[tuple[Path, int]] = []
    missing_endpoint: list[Path] = []
    missing_test: list[Path] = []
    missing_digest: list[Path] = []
    bad_digest: list[Path] = []
    backfilled = 0
    malformed: list[Path] = []
    markers: list[Path] = []
    secrets: list[Path] = []
    suite_sizes: dict[str, int] = {}
    total_size = 0

    for path in files:
        suite = path.parent.name
        size = path.stat().st_size
        total_size += size
        suite_sizes[suite] = suite_sizes.get(suite, 0) + size
        text = path.read_text(encoding="utf-8", errors="replace")
        for pattern in SECRET_PATTERNS:
            if pattern.search(text):
                secrets.append(path)
                break
        if path.name.startswith("_missing_"):
            markers.append(path)
            continue
        try:
            document = json.loads(text)
        except json.JSONDecodeError as error:
            malformed.append(path)
            errors.append(f"malformed JSON: {path.relative_to(ROOT)}: {error}")
            continue
        metadata = metadata_of(document)
        recorded_at = parse_time(metadata.get("recorded_at"))
        if recorded_at is None:
            missing_metadata.append(path)
            continue
        age = (now - recorded_at).days
        if age > max_age:
            stale.append((path, age))
        if metadata.get("backfilled"):
            backfilled += 1
            continue
        if not metadata.get("endpoint_host"):
            missing_endpoint.append(path)
        if not metadata.get("test"):
            missing_test.append(path)
        digest = metadata.get("system_sha256")
        if digest is None:
            missing_digest.append(path)
        elif not DIGEST_PATTERN.fullmatch(str(digest)):
            bad_digest.append(path)

    print(f"cassettes: {len(files)} files, {human_size(total_size)}, {len(suite_sizes)} suites")
    if backfilled:
        print(f"  backfilled metadata: {backfilled} (endpoint/test were not recorded)")
    for suite in sorted(suite_sizes):
        print(f"  {suite:<22} {human_size(suite_sizes[suite])}")
        if suite_sizes[suite] > max_bytes:
            warnings.append(
                f"suite {suite} is {human_size(suite_sizes[suite])} (budget {args.max_mb} MiB)"
            )
    if total_size > max_bytes:
        warnings.append(
            f"cassette tree is {human_size(total_size)} (budget {args.max_mb} MiB)"
        )
    if missing_metadata:
        warnings.append(
            f"{len(missing_metadata)} recordings have no recorded_at; run --backfill"
        )
    if markers:
        warnings.append(
            f"{len(markers)} unmatched-request markers; run --prune to remove them"
        )
    if missing_endpoint:
        warnings.append(
            f"{len(missing_endpoint)} recordings have no endpoint_host (backfilled or older)"
        )
    if missing_test:
        warnings.append(f"{len(missing_test)} recordings have no test name")
    if stale:
        warnings.append(
            f"{len(stale)} recordings are older than {max_age} days; oldest "
            f"{max(age for _, age in stale)} days"
        )
    if missing_digest:
        warnings.append(
            f"{len(missing_digest)} recordings predate the system-prompt digest; "
            "replay cannot detect prompt drift for them until they are re-recorded"
        )
    if bad_digest:
        errors.append(
            f"{len(bad_digest)} recordings have a malformed system_sha256"
        )
    if malformed:
        errors.append(f"{len(malformed)} malformed cassette files")
    if secrets:
        errors.append(f"{len(secrets)} files contain a credential-like string")
        for path in secrets[:10]:
            print(f"  secret: {path.relative_to(ROOT)}", file=sys.stderr)

    for message in warnings:
        print(f"warning: {message}", file=sys.stderr)
    for message in errors:
        print(f"error: {message}", file=sys.stderr)

    strict = args.strict or os.environ.get("CASSETTE_STRICT", "") not in ("", "0", "false", "no")
    if errors or (strict and warnings):
        return 1
    return 0


def git_date(path: Path) -> str | None:
    try:
        result = subprocess.run(
            ["git", "log", "-1", "--format=%cs", "--", str(path.relative_to(ROOT))],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return None
    value = result.stdout.strip()
    return value or None


def backfill(args: argparse.Namespace) -> int:
    if getattr(args, "digest", False):
        return backfill_digests()
    files = cassette_files()
    changed = 0
    for path in files:
        if path.name.startswith("_missing_"):
            continue
        try:
            document = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        metadata = document.get("metadata")
        if not isinstance(metadata, dict):
            metadata = {}
            document["metadata"] = metadata
        before = dict(metadata)
        if not metadata.get("recorded_at"):
            date = None if args.no_git_date else git_date(path)
            if date:
                metadata["recorded_at"] = f"{date}T00:00:00Z"
            else:
                stamp = dt.datetime.fromtimestamp(
                    path.stat().st_mtime, tz=dt.timezone.utc
                )
                metadata["recorded_at"] = stamp.replace(microsecond=0).isoformat().replace(
                    "+00:00", "Z"
                )
        if not metadata.get("suite"):
            metadata["suite"] = path.parent.name
        if not metadata.get("model") or metadata.get("model") == "<model>":
            model = document.get("request", {}).get("model")
            if isinstance(model, str) and model and model != "<model>":
                metadata["model"] = model
            else:
                metadata.pop("model", None)
        metadata["backfilled"] = True
        if metadata != before:
            path.write_text(
                json.dumps(document, indent=2, ensure_ascii=False) + "\n",
                encoding="utf-8",
            )
            changed += 1
    print(f"backfilled metadata for {changed} of {len(files)} cassette files")
    return 0


def backfill_digests() -> int:
    """Replay every suite and record each request's system-prompt digest.

    The digest cannot be recomputed from a cassette: canonicalization replaced
    the system prompt with a placeholder before the request was stored. It can
    be observed during replay, which is what this does — no provider calls.
    """
    updated = 0
    for suite in sorted(SUITES):
        directory = CASSETTES / suite
        if not directory.is_dir():
            continue
        print(f"tracing {suite}")
        with tempfile.NamedTemporaryFile("w+", suffix=".trace", delete=False) as handle:
            trace = Path(handle.name)
        try:
            if not run_suite(suite, trace, rewriting_digests=True):
                print(
                    f"warning: {suite} did not replay cleanly; digests not recorded for it",
                    file=sys.stderr,
                )
                continue
            text = trace.read_text(encoding="utf-8", errors="replace")
        finally:
            trace.unlink(missing_ok=True)
        digests: dict[str, str] = {}
        for line in text.splitlines():
            key = re.search(r"key=([0-9a-fA-F]+)", line)
            digest = re.search(r"digest=([0-9a-f]{64})", line)
            if key and digest:
                digests[key.group(1)] = digest.group(1)
        for path in sorted(directory.glob("*.json")):
            if path.name.startswith("_missing_"):
                continue
            digest = digests.get(path.stem)
            if digest is None:
                continue
            document = json.loads(path.read_text(encoding="utf-8"))
            if metadata_of(document).get("system_sha256") == digest:
                continue
            document.setdefault("metadata", {})["system_sha256"] = digest
            path.write_text(
                json.dumps(document, indent=2, ensure_ascii=False) + "\n",
                encoding="utf-8",
            )
            updated += 1
    print(f"recorded the system-prompt digest for {updated} cassette files")
    return 0


def fingerprint(path: str, canonical: dict) -> str:
    """FNV-1a over the path and compact JSON, exactly as `llm-cassette` does."""
    payload = (
        path.encode("utf-8")
        + b"\0"
        + json.dumps(canonical, ensure_ascii=False, separators=(",", ":")).encode(
            "utf-8"
        )
    )
    value = 0xCBF29CE484222325
    for byte in payload:
        value ^= byte
        value = (value * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{value:016x}"


def rekey_from_missing(args: argparse.Namespace) -> int:
    """Rekey cassettes from the requests a replay actually asked for.

    `rekey --template` cannot express "the main turn's ceiling moved but the
    side queries' did not": the template is one request, and a blanket
    `--set` would rewrite conversations it has nothing to do with. A replay
    with `CODAPE_CASSETTE_TRACE=1` records the exact current request for
    every conversation that missed, as `_missing_<new-key>.json` in the
    suite's directory. Each of those has a key the client now computes, so
    the work is to find the recording it describes and rename it.

    A recording matches a trace when the two agree on everything that
    identifies the conversation — the messages and the system prompt — and
    differ only in the request-wide and tool fields the trace supersedes.
    Ambiguity is an error: two recordings that both match would make the
    rename depend on directory order.
    """
    suites = args.suites or sorted(SUITES)
    rekeyed = 0
    for suite in suites:
        directory = CASSETTES / suite
        if not directory.is_dir():
            continue
        traces = []
        for path in sorted(directory.glob("_missing_*.json")):
            trace = json.loads(path.read_text(encoding="utf-8"))
            if not isinstance(trace, dict):
                continue
            # The filename carries the key; recomputing it is what catches a
            # trace edited by hand or written by a different canonicalizer.
            if fingerprint("/v1/messages", trace) != path.stem.removeprefix("_missing_"):
                print(
                    f"error: {path.relative_to(ROOT)} does not match its name",
                    file=sys.stderr,
                )
                return 1
            traces.append((path, trace))
        if not traces:
            continue
        traces = drop_stream_fallbacks(traces)
        if not traces:
            continue
        print(f"rekeying {suite} from {len(traces)} traced request(s)")
        cassettes = []
        for path in sorted(directory.glob("*.json")):
            if path.name.startswith("_missing_"):
                continue
            document = json.loads(path.read_text(encoding="utf-8"))
            request = document.get("request")
            if isinstance(request, dict):
                cassettes.append((path, document, request))
        used: set[Path] = set()
        for trace_path, trace in traces:
            matches = [
                (path, document, request)
                for path, document, request in cassettes
                if path not in used
                and conversation_identity(request) == conversation_identity(trace)
                # A recording of an event stream cannot answer a request that
                # asked for a JSON body, or the other way round: the client
                # would receive a body it cannot parse.
                and answers_a_stream(document) == is_streaming(trace)
            ]
            if not matches:
                print(
                    f"note: no recording matches {trace_path.name}",
                    file=sys.stderr,
                )
                continue
            if len(matches) > 1:
                names = ", ".join(str(path.relative_to(ROOT)) for path, _, _ in matches)
                print(
                    f"error: several recordings match {trace_path.name}: {names}",
                    file=sys.stderr,
                )
                return 1
            path, document, _ = matches[0]
            new_key = trace_path.stem.removeprefix("_missing_")
            target = directory / f"{new_key}.json"
            if args.dry_run:
                print(f"would rekey {path.relative_to(ROOT)} -> {new_key}")
                used.add(path)
                rekeyed += 1
                continue
            document["key"] = new_key
            document["request"] = trace
            target.write_text(
                json.dumps(document, indent=2, ensure_ascii=False) + "\n",
                encoding="utf-8",
            )
            if target != path:
                path.unlink()
            used.add(path)
            rekeyed += 1
    verb = "would rekey" if args.dry_run else "rekeyed"
    print(f"{verb} {rekeyed} cassette files")
    return 0


def conversation_identity(request: dict) -> str:
    """The part of a request that identifies which conversation it answers.

    Everything else in a recorded body — `max_tokens`, the tool definitions,
    the model, the metadata — can change between recordings of the same
    conversation; the messages and the system prompt cannot.
    """
    return json.dumps(
        {
            "messages": request.get("messages"),
            "system": request.get("system"),
        },
        sort_keys=True,
        ensure_ascii=False,
    )


def is_streaming(request: dict) -> bool:
    """Whether the request asks for an event stream.

    Absent and `false` are the same thing: the non-streaming path in
    `api_client` does not write the field at all.
    """
    return request.get("stream") is True


def answers_a_stream(cassette: dict) -> bool:
    """Whether the recording holds an event stream rather than a JSON body."""
    responses = cassette.get("responses") or []
    for response in responses:
        content_type = response.get("content_type") if isinstance(response, dict) else None
        if isinstance(content_type, str) and "event-stream" in content_type:
            return True
    return False


def drop_stream_fallbacks(traces: list[tuple[Path, dict]]) -> list[tuple[Path, dict]]:
    """Remove the non-streaming retry that a missed stream produces.

    `engine::send_request_streaming` retries the same turn without streaming
    when the stream yields nothing usable — and a replayed request that misses
    yields exactly that. The retry carries the same messages and system prompt
    as the request that missed, so the two traces look like one conversation
    asking for two recordings; the recording set only ever holds the streaming
    one, because the fallback never happened when the cassettes were recorded.
    Rekeying both would make each pass rename the same recording to the other
    key, which is why the earlier runs reported 50, 49, 48 and never reached
    zero.
    """
    streaming = {
        conversation_identity(trace)
        for _, trace in traces
        if is_streaming(trace)
    }
    kept = []
    for path, trace in traces:
        if not is_streaming(trace) and conversation_identity(trace) in streaming:
            print(
                f"note: {path.name} is the non-streaming retry of a request that "
                f"already has a trace; dropping it",
                file=sys.stderr,
            )
            path.unlink()
            continue
        kept.append((path, trace))
    return kept


def rekey(args: argparse.Namespace) -> int:
    """Refresh recorded request fields from a current traced request.

    A recording's key is the hash of its canonical request body, and that body
    carries the tool schemas and the request-wide settings. Adding a tool
    parameter, or raising `max_tokens`, therefore changes the key and makes
    replay miss even though the conversation is unchanged. Re-recording would
    answer the same conversations again; this instead rewrites the named fields
    from a request observed during replay (`CODAPE_CASSETTE_TRACE=1` writes it
    as `_missing_<key>.json`) and renames each cassette to the key the client
    now computes.

    Only the fields in `--set` change. Everything else stays exactly as
    recorded, so a rekey cannot quietly alter a conversation.
    """
    template_path = Path(args.template)
    try:
        template = json.loads(template_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(f"error: cannot read {template_path}: {error}", file=sys.stderr)
        return 2
    template_tools = {
        tool["name"]: tool
        for tool in template.get("tools", [])
        if isinstance(tool, dict) and isinstance(tool.get("name"), str)
    }
    if not template_tools:
        print(f"error: {template_path} has no tools", file=sys.stderr)
        return 2
    overwrite = sorted(args.set or [])
    for name in overwrite:
        if name not in template:
            print(
                f"error: {template_path} has no field {name!r} to copy",
                file=sys.stderr,
            )
            return 2

    suites = args.suites or sorted(SUITES)
    rekeyed = 0
    for suite in suites:
        directory = CASSETTES / suite
        if not directory.is_dir():
            continue
        print(f"rekeying {suite}")
        for path in sorted(directory.glob("*.json")):
            if path.name.startswith("_missing_"):
                continue
            document = json.loads(path.read_text(encoding="utf-8"))
            request = document.get("request")
            if not isinstance(request, dict):
                continue
            # The stored name must describe the stored request. A disagreement
            # means a different path or canonicalization made the file, and
            # rewriting it would paper over that difference.
            stored_key = fingerprint("/v1/messages", request)
            if path.stem != stored_key:
                print(
                    f"error: {path.relative_to(ROOT)} does not match its key "
                    f"({stored_key}); refusing to rekey",
                    file=sys.stderr,
                )
                return 1
            tools = request.get("tools")
            changed = False
            if isinstance(tools, list):
                refreshed = []
                for tool in tools:
                    name = tool.get("name") if isinstance(tool, dict) else None
                    replacement = template_tools.get(name)
                    if replacement is not None and tool != replacement:
                        refreshed.append(replacement)
                        changed = True
                    else:
                        refreshed.append(tool)
                if changed:
                    request["tools"] = refreshed
            for name in overwrite:
                if request.get(name) != template[name]:
                    request[name] = template[name]
                    changed = True
            if not changed:
                continue
            new_key = fingerprint("/v1/messages", request)
            target = directory / f"{new_key}.json"
            if target != path and target.exists():
                print(
                    f"error: {target.relative_to(ROOT)} already exists",
                    file=sys.stderr,
                )
                return 1
            document["key"] = new_key
            if args.dry_run:
                print(f"would rekey {path.relative_to(ROOT)} -> {new_key}")
            else:
                target.write_text(
                    json.dumps(document, indent=2, ensure_ascii=False) + "\n",
                    encoding="utf-8",
                )
                if target != path:
                    path.unlink()
            rekeyed += 1
    verb = "would rekey" if args.dry_run else "rekeyed"
    print(f"{verb} {rekeyed} cassette files")
    return 0


def run_suite(suite: str, trace: Path, rewriting_digests: bool = False) -> bool:
    environment = dict(os.environ)
    environment.pop("CODAPE_CASSETTE_MODE", None)
    environment["CODAPE_CASSETTE_TRACE"] = str(trace)
    if rewriting_digests:
        # The old digest is expected to disagree while every one is rewritten.
        environment["CODAPE_CASSETTE_DIGEST_BACKFILL"] = "1"
    for binary, name_filter in SUITES[suite]:
        command = ["cargo", "test", "-p", "codape-cli", "--test", binary]
        if name_filter:
            command.append(name_filter)
        command.extend(["--", "--test-threads=1"])
        print(f"  replay {binary}" + (f" {name_filter}" if name_filter else ""))
        result = subprocess.run(
            command,
            cwd=ROOT,
            env=environment,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            print(
                f"error: {binary} failed; not pruning {suite}\n{result.stderr[-2000:]}",
                file=sys.stderr,
            )
            return False
    return True


def prune(args: argparse.Namespace) -> int:
    removed = 0
    missing: list[str] = []
    for suite in sorted(SUITES):
        directory = CASSETTES / suite
        if not directory.is_dir():
            continue
        print(f"tracing {suite}")
        with tempfile.NamedTemporaryFile("w+", suffix=".trace", delete=False) as handle:
            trace = Path(handle.name)
        try:
            if not run_suite(suite, trace):
                continue
            text = trace.read_text(encoding="utf-8", errors="replace")
        finally:
            trace.unlink(missing_ok=True)
        used = set(re.findall(r"key=([0-9a-fA-F]+)", text))
        if not used:
            print(f"warning: no cassette requests traced for {suite}; skipped", file=sys.stderr)
            continue
        files = {path.stem: path for path in directory.glob("*.json")}
        marker_keys = {
            path.stem.removeprefix("_missing_")
            for path in directory.glob("_missing_*.json")
        }
        for path in directory.glob("_missing_*.json"):
            if args.dry_run:
                print(f"would delete {path.relative_to(ROOT)}")
            else:
                path.unlink()
                removed += 1
        for key, path in sorted(files.items()):
            if key in used or key.startswith("_missing_"):
                continue
            if args.dry_run:
                print(f"would delete {path.relative_to(ROOT)}")
            else:
                path.unlink()
                removed += 1
        for key in sorted(used - set(files) - marker_keys):
            missing.append(f"{suite}/{key}.json")
        print(f"  {len(used)} keys used, {len(files)} files")
    for path in missing:
        print(f"error: trace used a cassette that does not exist: {path}", file=sys.stderr)
    if args.dry_run:
        print("dry run: no files were deleted")
    else:
        print(f"pruned {removed} cassette files")
    return 1 if missing else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    check_parser = subparsers.add_parser("check")
    check_parser.add_argument(
        "--max-age-days",
        type=int,
        default=int(os.environ.get("CASSETTE_MAX_AGE_DAYS", "180")),
    )
    check_parser.add_argument(
        "--max-mb", type=int, default=int(os.environ.get("CASSETTE_MAX_MB", "30"))
    )
    check_parser.add_argument("--strict", action="store_true")
    check_parser.set_defaults(func=check)

    backfill_parser = subparsers.add_parser("backfill")
    backfill_parser.add_argument(
        "--no-git-date",
        action="store_true",
        help="use file mtime instead of the file's most recent git commit date",
    )
    backfill_parser.add_argument(
        "--digest",
        action="store_true",
        help="replay the suites and record the system-prompt digest of each request "
        "(recordings made before the field existed get it without a re-record)",
    )
    backfill_parser.set_defaults(func=backfill)

    rekey_parser = subparsers.add_parser("rekey")
    rekey_parser.add_argument(
        "--template",
        help="a request traced during replay (_missing_<key>.json) whose tool "
        "definitions are the current ones",
    )
    rekey_parser.add_argument(
        "--missing",
        action="store_true",
        help="rekey from every _missing_<key>.json a traced replay left behind, "
        "matching each to the recording it describes (no --template needed)",
    )
    rekey_parser.add_argument("--dry-run", action="store_true")
    rekey_parser.add_argument(
        "--set",
        action="append",
        metavar="FIELD",
        help="also copy this request field from the template (repeatable); use "
        "for request-wide settings such as max_tokens",
    )
    rekey_parser.add_argument(
        "suites",
        nargs="*",
        help="suites to rewrite (default: every known suite)",
    )
    rekey_parser.set_defaults(func=rekey)

    prune_parser = subparsers.add_parser("prune")
    prune_parser.add_argument("--dry-run", action="store_true")
    prune_parser.set_defaults(func=prune)

    args = parser.parse_args()
    if args.func is rekey:
        if args.missing:
            return rekey_from_missing(args)
        if not args.template:
            rekey_parser.error("--template is required unless --missing is given")
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
