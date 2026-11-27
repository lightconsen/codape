#!/usr/bin/env python3
"""Keep the landing page's advertised version equal to the crate's.

`site/src/data/site.ts` carries a `VERSION` constant that the hero chip and the
terminal animation both display, and `codape --version` prints the version from
`crates/cli/Cargo.toml`. Nothing connected the two: the site carried `0.2.2`
and there was nothing to notice when a bump left it behind, because a release
commit edits twenty-one manifests and a lockfile and has no reason to look at
TypeScript.

The two literals also drifted from each other in the other direction: the
terminal animation hardcoded `codape v0.2.2` in a second string, so a bump that
found the `VERSION` constant would still have left a stale copy on the page. It
reads the constant now, and this check asserts that nothing under `site/src`
repeats a version literal of its own.

Usage:
    python3 -I scripts/check-site-version.py
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "crates" / "cli" / "Cargo.toml"
SITE_VERSION = ROOT / "site" / "src" / "data" / "site.ts"
SITE_SRC = ROOT / "site" / "src"

FAILURES: list[str] = []


def check(condition: bool, message: str) -> None:
    if not condition:
        FAILURES.append(message)


def crate_version() -> str | None:
    text = MANIFEST.read_text(encoding="utf-8")
    match = re.search(r'^version = "([^"]+)"', text, re.MULTILINE)
    return match.group(1) if match else None


def site_version() -> str | None:
    text = SITE_VERSION.read_text(encoding="utf-8")
    match = re.search(r'export const VERSION = "([^"]+)"', text)
    return match.group(1) if match else None


def repeated_versions(version: str) -> list[str]:
    """Files under `site/src` that spell the version out instead of reading it.

    The constant's own definition is excluded; so is anything that is not a
    source file, because the build output and the lockfile carry versions for
    reasons of their own.
    """
    hits = []
    # Not `\b`: the literal that drifted was spelled `codape v0.2.2`, and `v`
    # and `0` are both word characters, so a word boundary never fires there.
    # A digit or a dot on either side means the run is longer than this version
    # (`0.2.30`), which is a different version rather than this one repeated.
    source = re.compile(rf"(?<![\d.]){re.escape(version)}(?![\d.])")
    for path in sorted(SITE_SRC.rglob("*")):
        if path.suffix not in {".ts", ".tsx", ".css"}:
            continue
        if path == SITE_VERSION:
            continue
        if source.search(path.read_text(encoding="utf-8", errors="replace")):
            hits.append(str(path.relative_to(ROOT)))
    return hits


def main() -> int:
    crate = crate_version()
    site = site_version()
    if crate is None:
        print(f"could not read a version from {MANIFEST}", file=sys.stderr)
        return 1
    if site is None:
        print(f"could not read VERSION from {SITE_VERSION}", file=sys.stderr)
        return 1

    check(
        crate == site,
        f"the site advertises {site} and the crate is {crate}; bump "
        f"site/src/data/site.ts in the same change as the manifests",
    )

    # Against the crate's version, not the site's: when they disagree the
    # first check already says so, and a stale literal copied from the old
    # release is the case this one exists to catch.
    for path in repeated_versions(crate):
        check(
            False,
            f"{path} spells out the version {crate!r}; read `VERSION` from "
            "site/src/data/site.ts so the next bump cannot miss it",
        )

    if FAILURES:
        for failure in FAILURES:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print(f"site version: {site}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
