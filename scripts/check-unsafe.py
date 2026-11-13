#!/usr/bin/env python3
"""Keep the workspace-wide `unsafe_code = "deny"` honest.

`Cargo.toml` denies `unsafe_code` for every crate, which is the strongest form
the lint has. It is also bypassable: a file that starts with
`#![allow(unsafe_code)]` compiles its `unsafe` blocks without a word, and
nothing in the build reports that the deny was switched off for it.

This is the check that makes the bypass visible. The files allowed to opt out
are listed in `scripts/unsafe_allowlist.txt` with the reason each one needs
`unsafe` at all. A pull request that adds a file-level allow without adding a
line there fails, and so does one that removes an allow without removing the
line — a list that can rot stops being a record of anything.

Item-level `#[allow(unsafe_code)]` (a single function, not the whole file) is
counted and reported but not gated: it is a narrower decision that the reader
sees next to the code it covers.

Usage: scripts/check-unsafe.py [--update]
       --update  rewrite the allowlist from the tree (review the diff before
                 committing it: this is what records the decision)
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ALLOWLIST = ROOT / "scripts" / "unsafe_allowlist.txt"
CRATES = ROOT / "crates"

# `#![allow(unsafe_code)]`, with or without other lints on the same attribute.
FILE_ALLOW = re.compile(r"^\s*#!\[\s*allow\([^)]*\bunsafe_code\b[^)]*\)\s*\]")
ITEM_ALLOW = re.compile(r"^\s*#\[\s*allow\([^)]*\bunsafe_code\b[^)]*\)\s*\]")
HEADER = "# Files allowed to switch off the workspace-wide unsafe_code deny."


def rust_files() -> list[Path]:
    return sorted(
        path
        for path in CRATES.rglob("*.rs")
        if "target" not in path.parts
    )


def scan() -> tuple[dict[str, list[str]], dict[str, list[str]]]:
    """Return (file-level allows, item-level allows) as path -> lines."""
    files: dict[str, list[str]] = {}
    items: dict[str, list[str]] = {}
    for path in rust_files():
        text = path.read_text(encoding="utf-8", errors="replace")
        for number, line in enumerate(text.splitlines(), 1):
            relative = str(path.relative_to(ROOT))
            if FILE_ALLOW.match(line):
                files.setdefault(relative, []).append(str(number))
            elif ITEM_ALLOW.match(line):
                items.setdefault(relative, []).append(str(number))
    return files, items


def read_allowlist() -> dict[str, str]:
    """Return path -> recorded reason. A blank reason is an error, not a pass."""
    if not ALLOWLIST.is_file():
        return {}
    recorded: dict[str, str] = {}
    for line in ALLOWLIST.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        path, _, reason = line.partition(" ")
        recorded[path] = reason.strip()
    return recorded


def write_allowlist(found: dict[str, list[str]]) -> None:
    existing = read_allowlist()
    lines = [
        HEADER,
        "#",
        "# Checked by scripts/check-unsafe.py, which fails when a file switches the",
        "# lint off without a line here, and when a line here no longer matches a",
        "# file. Each reason says why the deny cannot hold for that file. Add one",
        "# deliberately: this file is the record of the decision, so the diff that",
        "# adds it is what gets reviewed.",
        "#",
        "# format: <path> <reason>",
        "",
    ]
    for path in sorted(found):
        lines.append(f"{path} {existing.get(path, 'TODO: why this file needs unsafe')}")
    ALLOWLIST.write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--update",
        action="store_true",
        help="rewrite the allowlist from the current tree",
    )
    args = parser.parse_args()

    found, items = scan()
    if args.update:
        write_allowlist(found)
        print(f"wrote {len(found)} entries to {ALLOWLIST.relative_to(ROOT)}")
        return 0

    recorded = read_allowlist()
    problems: list[str] = []

    for path in sorted(set(found) - set(recorded)):
        problems.append(
            f"{path} switches off unsafe_code but is not in "
            f"{ALLOWLIST.relative_to(ROOT)}"
        )
    for path in sorted(set(recorded) - set(found)):
        problems.append(
            f"{ALLOWLIST.relative_to(ROOT)} lists {path}, which no longer "
            "switches unsafe_code off — remove the line"
        )
    for path, reason in sorted(recorded.items()):
        if not reason:
            problems.append(f"{path} has no reason recorded next to it")

    print(f"file-level unsafe_code allows: {len(found)} (allowlist: {len(recorded)})")
    for path, lines in sorted(found.items()):
        print(f"  {path}:{','.join(lines)}")
    if items:
        total = sum(len(lines) for lines in items.values())
        print(f"item-level unsafe_code allows: {total} in {len(items)} file(s)")

    for problem in problems:
        print(f"error: {problem}", file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
