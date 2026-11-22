#!/usr/bin/env python3
"""Rebuild the `0.1.1-analysis` dataset copy from the pristine one.

`docs/terminal-bench.md#local-fixture-patches` records 26 edits across 22
Terminal-Bench tasks. Most are prose rewrites of an instruction, which no
script can invent. The rest are mechanical — a flag on a `patch` call, a wrong
constant, a package name, a missing space — and those are here, with their
exact old text, because a fixture defect that is only described in prose is a
fixture defect that gets re-made by the next person who refreshes the cache.

The measured consequence of leaving one alone: `swe-bench-fsspec`'s grading
script applies the gold test patch with `patch --fuzz=5 -p1`. When the agent
has already written the same test, `patch` detects a reverse-or-applied patch
and **asks**, on a terminal that nobody is typing into, until the harness's
test budget runs out — 0/3 on 2026-11-10, recorded as `test_timeout` rather
than a verdict. `-N` turns that question into an immediate skip.

That detail hid for months because it only reproduces on a terminal. With
stdin at end-of-file, `patch` answers its own question and exits; under a PTY
it blocks. The self-test here therefore runs `patch` both ways and asserts the
difference, so the reason the flag is there cannot quietly stop being true.

Every edit must match its anchor **exactly once**. A fixture that has drifted
fails loudly instead of being silently half-patched, which is the failure mode
this script exists to prevent.

Usage:
    python3 -I scripts/external-benchmarks/fixture_patches.py verify
    python3 -I scripts/external-benchmarks/fixture_patches.py verify --dataset PATH
    python3 -I scripts/external-benchmarks/fixture_patches.py build \\
        --source ~/.cache/terminal-bench/terminal-bench-core/0.1.1 \\
        --dest ~/.cache/terminal-bench/terminal-bench-core/0.1.1-analysis
    python3 -I scripts/external-benchmarks/fixture_patches.py self-test
"""

from __future__ import annotations

import argparse
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

DEFAULT_DATASET = Path.home() / ".cache" / "terminal-bench" / "terminal-bench-core" / "0.1.1"

SCALING = (
    "  The model was trained on raw pixel bytes: scale each pixel as "
    "`p / 255.0` and do not centre or normalize further."
)

PATCH_CALL = "patch --fuzz=5 -p1 -i /app/test_patch.diff"


@dataclass(frozen=True)
class Edit:
    """One anchor in one file.

    `marker` is what proves the edit is already applied. It cannot be inferred
    from `new`, because an insertion keeps its anchor line and would look
    applied forever; and it cannot be inferred from `old` disappearing, because
    one edit's old text is a prefix of its own new text (`15 * 1024 * 1024`
    versus `15 * 1024 * 1024 * 1024`). Both mistakes were live here until the
    self-test caught them.
    """

    path: str
    old: str
    new: str
    marker: str
    why: str


EDITS: tuple[Edit, ...] = (
    # --- the gold test patch must not wait for an answer --------------------
    *(
        Edit(
            path=f"{task}/run-tests.sh",
            old=PATCH_CALL,
            new="patch --fuzz=5 -N -p1 -i /app/test_patch.diff",
            marker="patch --fuzz=5 -N -p1",
            why="`patch` asks `Reversed (or previously applied) patch "
            "detected!` when the agent has already written the test the gold "
            "patch adds. The grading script runs in a tmux pane, so the "
            "question blocks until the whole test budget is gone. `-N` skips "
            "an applied patch and exits 1; the script has no `set -e`, so "
            "grading continues and the gold test is already in place.",
        )
        for task in (
            "swe-bench-fsspec",
            "swe-bench-langcodes",
            "swe-bench-astropy-1",
            "swe-bench-astropy-2",
        )
    ),
    # --- a size assertion that could never fail -----------------------------
    Edit(
        path="reshard-c4-data/tests/test_outputs.py",
        old="MAX_SIZE_PER_FILE = 15 * 1024 * 1024 * 1024",
        new="MAX_SIZE_PER_FILE = 15 * 1024 * 1024",
        marker="MAX_SIZE_PER_FILE = 15 * 1024 * 1024\n",
        why="15 GiB, not the 15 MiB the task asks for, so the size check could "
        "never fail.",
    ),
    # --- two images that no longer build ------------------------------------
    *(
        Edit(
            path=f"{task}/Dockerfile",
            old="FROM debian:bullseye-slim ",
            new="FROM debian:bookworm-slim ",
            marker="FROM debian:bookworm-slim ",
            why="`debian:bullseye-slim` pins a qemu package withdrawn upstream; "
            "the image does not build, so no trial exists at all.",
        )
        for task in ("qemu-alpine-ssh", "qemu-startup")
    ),
    *(
        Edit(
            path=f"{task}/Dockerfile",
            old="telnet netcat expect tmux asciinema",
            new="telnet netcat-openbsd expect tmux asciinema",
            marker="netcat-openbsd",
            why="`netcat` is a virtual package with no default, so the install "
            "fails and takes the build with it.",
        )
        for task in ("qemu-alpine-ssh", "qemu-startup")
    ),
    # --- two ssh arguments that concatenate ---------------------------------
    Edit(
        path="qemu-alpine-ssh/tests/test_outputs.py",
        old="""        + '-p 2222 root@localhost "uname -r"'""",
        new="""        + ' -p 2222 root@localhost "uname -r"'""",
        marker="""        + ' -p 2222""",
        why='The two literals concatenate to `...no-p 2222`, which ssh rejects '
        'as `unsupported option "no-p"`, so the test fails for any correct '
        "agent.",
    ),
    # --- the weights file the image actually ships --------------------------
    *(
        Edit(
            path=f"{task}/task.yaml",
            old='called "simple_mnist.pth"',
            new='called "model.pth"',
            marker='called "model.pth"',
            why="The image ships `model.pth`; `simple_mnist.pth` does not exist, "
            "so an agent that follows the instruction looks for a missing file.",
        )
        for task in ("pytorch-model-cli", "pytorch-model-cli.easy")
    ),
    # --- the scaling the test scores but the instruction never states -------
    *(
        Edit(
            path=f"{task}/task.yaml",
            old=anchor,
            new=f"{anchor}\n\n{SCALING}",
            marker="scale each pixel as `p / 255.0`",
            why="The test's `TRUE_PRED_CLASSES` are the reference's `x/255` "
            "predictions, including 15 of its mistakes. An agent that centres "
            "the pixels is *more* accurate (43/50 against 35/50) and still "
            "fails, and the instruction never named a scaling, so this was "
            "graded as a model failure on an unstated requirement.",
        )
        # The two variants read differently at this point, so each gets the
        # paragraph it actually has. Both occur exactly once.
        for task, anchor in (
            (
                "pytorch-model-cli",
                '  Finally you are given an image called "image.png" which is a '
                "28x28 grayscale image of a handwritten digit to check your "
                "implementation.",
            ),
            (
                "pytorch-model-cli.easy",
                "  Also the model has an input dimension of 784 a hidden "
                "dimension of 16 and an output dimension of 10 as well as a "
                "ReLU activation function.",
            ),
        )
    ),
)


class FixtureError(RuntimeError):
    pass


def _read(root: Path, relpath: str) -> str:
    try:
        return (root / relpath).read_text(encoding="utf-8")
    except OSError as exc:
        raise FixtureError(f"{relpath}: cannot read: {exc}") from exc


def edit(root: Path, entry: Edit) -> bool:
    """Apply one edit. Returns False when the copy already carries it.

    Raises when the anchor is missing or ambiguous: a half-applied fixture is
    worse than an unapplied one, because a run against it still produces a
    number that looks like a verdict.
    """
    text = _read(root, entry.path)
    if entry.marker in text:
        return False
    occurrences = text.count(entry.old)
    if occurrences == 0:
        raise FixtureError(
            f"{entry.path}: anchor not found: {entry.old.splitlines()[0][:70]!r}. "
            "The dataset revision differs from the one this table was written "
            "against; check the edit before trusting a run against this copy."
        )
    if occurrences > 1:
        raise FixtureError(
            f"{entry.path}: anchor appears {occurrences} times, so the edit is "
            f"ambiguous: {entry.old.splitlines()[0][:70]!r}"
        )
    (root / entry.path).write_text(
        text.replace(entry.old, entry.new, 1), encoding="utf-8"
    )
    return True


def apply(root: Path) -> list[str]:
    changed = []
    for entry in EDITS:
        if edit(root, entry):
            changed.append(f"{entry.path}: {entry.why}")
    return changed


def verify(root: Path) -> tuple[list[str], list[str], list[str]]:
    """Report which edits a copy carries. Nothing is written.

    Counted per *edit*, not per file: `pytorch-model-cli` takes two edits and a
    copy can carry one of them, which a file-level answer would have to report
    as both present and missing.
    """
    have: list[str] = []
    missing: list[str] = []
    absent: list[str] = []
    for entry in EDITS:
        try:
            text = _read(root, entry.path)
        except FixtureError:
            absent.append(entry.path)
            continue
        label = f"{entry.path}: {entry.marker.strip()!r}"
        (have if entry.marker in text else missing).append(label)
    return have, missing, sorted(set(absent))


def edited_files() -> set[str]:
    return {entry.path for entry in EDITS}


def cmd_verify(args: argparse.Namespace) -> int:
    root = Path(args.dataset).expanduser()
    if not root.is_dir():
        print(f"error: {root} is not a directory", file=sys.stderr)
        return 2
    have, missing, absent = verify(root)
    print(f"dataset: {root}")
    print(f"edits  : {len(EDITS)} anchors in {len(edited_files())} files")
    print(f"present: {len(have)} of {len(EDITS)}")
    failed = False
    for path in absent:
        print(f"  absent : {path} (this copy has no such task)")
    for label in missing:
        print(f"  missing: {label}")
        failed = True
    if failed:
        return 1
    print("every mechanical edit is present")
    return 0


def copy_tree(source: Path, dest: Path) -> None:
    if not source.is_dir():
        raise FixtureError(f"{source} is not a directory")
    if dest.exists() and any(dest.iterdir()):
        raise FixtureError(
            f"{dest} already exists and is not empty; refusing to overwrite a "
            "dataset copy"
        )
    shutil.copytree(source, dest, dirs_exist_ok=True)


def cmd_build(args: argparse.Namespace) -> int:
    source = Path(args.source).expanduser()
    dest = Path(args.dest).expanduser()
    try:
        copy_tree(source, dest)
        # The docs promise a `.orig` sibling holding the pristine text, so take
        # the copies before anything is edited.
        if not args.no_orig:
            for relpath in edited_files():
                live = dest / relpath
                orig = live.with_name(live.name + ".orig")
                if live.exists() and not orig.exists():
                    shutil.copy2(live, orig)
        changed = apply(dest)
    except FixtureError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    for line in changed:
        print(f"edited: {line}")
    print(f"\n{len(changed)} edits applied to {dest}")
    print(
        "the prose edits in docs/terminal-bench.md#local-fixture-patches that "
        "are not listed here still have to be applied by hand"
    )
    return 0


# ---------------------------------------------------------------------------
# The reason the -N flag is there, checked rather than asserted in prose
# ---------------------------------------------------------------------------

def patch_blocks_without_flag(directory: Path) -> bool:
    """Does `patch` ask, and wait, when the patch is already applied?

    A subprocess with stdin at end-of-file does not reproduce it: `patch`
    reads EOF, takes the default, and exits. The prompt only blocks when it is
    attached to a terminal, which is what the tmux pane is. This opens a real
    PTY for that reason; if `pty` is unavailable the check reports that and
    passes, so it never turns a platform gap into a false failure.
    """
    try:
        import pty
    except ImportError:  # Windows
        return False
    (directory / "f.txt").write_text("a\nNEW\nb\nc\n", encoding="utf-8")
    pid, fd = pty.fork()
    if pid == 0:  # child
        os.chdir(directory)
        os.execvp(
            "patch",
            ["patch", "--fuzz=5", "-p1", "-i", "p.diff"],
        )
    out = b""
    deadline = time.monotonic() + 5
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    chunk = os.read(fd, 4096)
                except OSError:
                    break
                if not chunk:
                    break
                out += chunk
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                return False
    finally:
        try:
            os.kill(pid, 9)
        except OSError:
            pass
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass
        os.close(fd)
    return b"Assume -R" in out


def cmd_self_test(_args: argparse.Namespace) -> int:
    failures: list[str] = []

    def check(condition: bool, message: str) -> None:
        if not condition:
            failures.append(message)

    with tempfile.TemporaryDirectory() as tmp:
        # 1. A clean copy takes every edit, and every edit is idempotent.
        base = Path(tmp) / "base"
        _synthetic_tree(base)
        changed = apply(base)
        check(len(changed) == len(EDITS), f"applied {len(changed)} of {len(EDITS)} edits")
        check(apply(base) == [], "a second apply must be a no-op")
        have, missing, absent = verify(base)
        check(missing == [], f"verify after apply reported missing: {missing}")
        check(absent == [], f"verify after apply found no such task: {absent}")
        check(len(have) == len(EDITS), f"verify found {len(have)} of {len(EDITS)} edits")

        # 2. Each edit lands where the docs say it does.
        fsspec = (base / "swe-bench-fsspec/run-tests.sh").read_text(encoding="utf-8")
        check("patch --fuzz=5 -N -p1" in fsspec, "the fsspec patch call lost -N")
        reshard = (base / "reshard-c4-data/tests/test_outputs.py").read_text(
            encoding="utf-8"
        )
        check(
            reshard.count("MAX_SIZE_PER_FILE = 15 * 1024 * 1024\n") == 1,
            f"reshard size is not 15 MiB exactly once: {reshard!r}",
        )
        for task in ("qemu-alpine-ssh", "qemu-startup"):
            dockerfile = (base / task / "Dockerfile").read_text(encoding="utf-8")
            check("bookworm-slim" in dockerfile, f"{task}: base image not bumped")
            check("netcat-openbsd" in dockerfile, f"{task}: netcat not replaced")
        ssh_test = (base / "qemu-alpine-ssh/tests/test_outputs.py").read_text(
            encoding="utf-8"
        )
        check("""+ ' -p 2222""" in ssh_test, "the ssh arguments still concatenate")
        for task in ("pytorch-model-cli", "pytorch-model-cli.easy"):
            text = (base / task / "task.yaml").read_text(encoding="utf-8")
            check('called "model.pth"' in text, f"{task}: weights file not renamed")
            check("p / 255.0" in text, f"{task}: scaling not stated")
            check("simple_mnist.pth" not in text, f"{task}: stale name left behind")

        # 3. A drifted copy fails instead of being half-patched. The anchor is
        #    changed to something that is not a substring of the old text:
        #    `15 * 1024 * 1024 * 1024` contains the MiB form, so a "drifted"
        #    copy built by appending to it would still match.
        drifted = Path(tmp) / "drifted"
        _synthetic_tree(drifted)
        (drifted / "swe-bench-fsspec/run-tests.sh").write_text(
            "patch --fuzz=5 -p1 --batch -i /app/test_patch.diff\n",
            encoding="utf-8",
        )
        try:
            apply(drifted)
        except FixtureError as exc:
            check("anchor not found" in str(exc), f"unexpected message: {exc}")
        else:
            failures.append("a missing anchor must raise, not be skipped")

        # 4. An ambiguous anchor is refused too.
        doubled = Path(tmp) / "doubled"
        _synthetic_tree(doubled)
        (doubled / "swe-bench-langcodes/run-tests.sh").write_text(
            f"{PATCH_CALL}\n" * 2, encoding="utf-8"
        )
        try:
            apply(doubled)
        except FixtureError as exc:
            check("ambiguous" in str(exc), f"unexpected message: {exc}")
        else:
            failures.append("an ambiguous anchor must raise, not guess")

        # 5. verify() on an unpatched copy reports nothing present.
        pristine = Path(tmp) / "pristine"
        _synthetic_tree(pristine)
        have, missing, _absent = verify(pristine)
        check(have == [], f"an unpatched copy must report no edits, got {have}")
        check(len(missing) == len(EDITS), "an unpatched copy must report all missing")

        # 5b. A file with two edits and only one applied is half-done, and
        #     verify() has to say so. Counted per file it could only be reported
        #     as both present and missing, which reads as a contradiction.
        partial = Path(tmp) / "partial"
        _synthetic_tree(partial)
        _read_edit = next(e for e in EDITS if e.path == "pytorch-model-cli/task.yaml")
        text = (partial / "pytorch-model-cli/task.yaml").read_text(encoding="utf-8")
        (partial / "pytorch-model-cli/task.yaml").write_text(
            text.replace(_read_edit.old, _read_edit.new, 1), encoding="utf-8"
        )
        have, missing, _absent = verify(partial)
        check(len(have) == 1, f"a half-edited file must report 1 present, got {have}")
        check(
            any("pytorch-model-cli/task.yaml" in m for m in missing),
            f"the unapplied half must be reported missing, got {missing}",
        )

        # 6. The -N flag is still what stops the prompt. Run backwards first:
        #    if the unprompted form ever stops blocking, the flag has lost its
        #    stated reason and this file's docstring is wrong.
        probe = Path(tmp) / "patch"
        probe.mkdir()
        (probe / "p.diff").write_text(
            "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,4 @@\n a\n+NEW\n b\n c\n",
            encoding="utf-8",
        )
        if shutil.which("patch") is None:
            print("note: patch not installed; skipped the prompt check")
        elif patch_blocks_without_flag(probe):
            (probe / "f.txt").write_text("a\nNEW\nb\nc\n", encoding="utf-8")
            before = (probe / "f.txt").read_text(encoding="utf-8")
            with_flag = subprocess.run(
                ["patch", "--fuzz=5", "-N", "-p1", "-i", "p.diff"],
                cwd=probe,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=20,
                check=False,
            )
            # What matters is what `-N` does, not what one implementation calls
            # it. The unprompted form blocked a moment ago; this one returned,
            # and it must have returned having left the file alone. Asserting
            # the notice's exact sentence is what broke: Apple's patch says
            # "Ignoring previously applied (or reversed) patch." and GNU's says
            # "Reversed (or previously applied) patch detected!" — so the check
            # passed on the machine it was written on and failed on Linux CI.
            check(
                (probe / "f.txt").read_text(encoding="utf-8") == before,
                "-N must leave an already-patched file untouched",
            )
            notice = (with_flag.stdout + with_flag.stderr).lower()
            check(
                "previously applied" in notice,
                f"-N must report the skip, got: {(with_flag.stdout + with_flag.stderr).strip()!r}",
            )
        else:
            print("note: patch did not block on a PTY here; skipped the -N check")

    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print("fixture patches: ok")
    return 0


def _synthetic_tree(root: Path) -> None:
    """Just enough of the dataset for every anchor to match exactly once.

    The prose around each anchor is the real thing where it is short and a stub
    where it is long, because what is under test is the anchor discipline, not
    the wording.
    """
    files = {
        f"{task}/run-tests.sh": f"{PATCH_CALL}\n"
        for task in (
            "swe-bench-fsspec",
            "swe-bench-langcodes",
            "swe-bench-astropy-1",
            "swe-bench-astropy-2",
        )
    }
    files.update(
        {
            "reshard-c4-data/tests/test_outputs.py": (
                "MAX_SIZE_PER_FILE = 15 * 1024 * 1024 * 1024\n"
            ),
            "qemu-alpine-ssh/Dockerfile": (
                "FROM debian:bullseye-slim \n"
                "RUN apt install -y telnet netcat expect tmux asciinema\n"
            ),
            "qemu-startup/Dockerfile": (
                "FROM debian:bullseye-slim \n"
                "RUN apt install -y telnet netcat expect tmux asciinema\n"
            ),
            "qemu-alpine-ssh/tests/test_outputs.py": (
                "    result = os.popen(\n"
                """        'sshpass -p "password123" ssh -o "StrictHostKeyChecking=no"'\n"""
                """        + '-p 2222 root@localhost "uname -r"'\n"""
                "    ).read()\n"
            ),
            "pytorch-model-cli/task.yaml": (
                '  a state_dict called "simple_mnist.pth".\n'
                '  Finally you are given an image called "image.png" which is a '
                "28x28 grayscale image of a handwritten digit to check your "
                "implementation.\n"
                "  Everything should be located in the /app directory.\n"
            ),
            "pytorch-model-cli.easy/task.yaml": (
                '  a state_dict called "simple_mnist.pth".\n'
                "  Also the model has an input dimension of 784 a hidden "
                "dimension of 16 and an output dimension of 10 as well as a "
                "ReLU activation function.\n"
                "  Everything should be located in the /app directory.\n"
            ),
        }
    )
    for relpath, text in files.items():
        path = root / relpath
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Apply the mechanical Terminal-Bench fixture edits.",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_verify = sub.add_parser("verify", help="report which edits a copy carries")
    p_verify.add_argument("--dataset", default=str(DEFAULT_DATASET))
    p_verify.set_defaults(func=cmd_verify)

    p_build = sub.add_parser("build", help="copy the pristine dataset and patch it")
    p_build.add_argument("--source", default=str(DEFAULT_DATASET))
    p_build.add_argument(
        "--dest",
        default=str(DEFAULT_DATASET.parent / "0.1.1-analysis"),
    )
    p_build.add_argument(
        "--no-orig",
        action="store_true",
        help="skip the .orig siblings the docs promise",
    )
    p_build.set_defaults(func=cmd_build)

    p_self = sub.add_parser("self-test", help="check the anchor rules on fixtures")
    p_self.set_defaults(func=cmd_self_test)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
