#!/usr/bin/env python3
"""Check the host-install and host-speed scripts without a benchmark host.

Both scripts exist for a machine that is rented and thrown away, so their
Linux-only branches are the ones nobody exercises: the Ubuntu package list, the
Docker install, the version gate that refuses a CLI too old for `--file`, and
the registry check that catches a mirror serving `alpine` but not
`harborframework/*`. A typo in any of them surfaces as a failed 16-hour sweep
or, worse, as a sweep that ran and measured the wrong host.

The interesting assertion is `check_installer_paths`: it runs the installer's
dry run on *this* machine, which is deliberately not the target, and requires
that it still walk every section — including the ones only a Linux host reaches
— before reporting the faults it found. That is what makes the dry run usable
as a pre-rental check, and it is the only way a CI box that is not the rented
host can execute the Docker-install branch at all.

Usage:
    python3 -I scripts/external-benchmarks/check_host_scripts.py
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SCRIPTS = ROOT / "terminal-bench"
INSTALLER = SCRIPTS / "install-benchmark-host.sh"
SPEED = SCRIPTS / "measure-host-speed.sh"

FAILURES: list[str] = []


def check(condition: bool, message: str) -> None:
    if not condition:
        FAILURES.append(message)


def run(script: Path, args: list[str], env: dict[str, str] | None = None):
    return subprocess.run(
        [str(script), *args],
        capture_output=True,
        text=True,
        env={**os.environ, **(env or {})},
        cwd=str(SCRIPTS),
    )


def check_shell(script: Path) -> None:
    """Both are bash and say so, and each still parses as bash.

    The runner was once executed with `sh` by its own check, which passed on a
    macOS whose `sh` tolerates arrays and failed on Ubuntu's dash. The same
    mistake here would mean a green check on the machine that is not the target
    and a red run on the one that is.
    """
    first = script.read_text(encoding="utf-8").splitlines()[0]
    check(
        first.startswith("#!") and first.rstrip().endswith("bash"),
        f"{script.name} must declare bash; it is parsed and run as bash",
    )
    check(
        "pipefail" in script.read_text(encoding="utf-8"),
        f"{script.name} must set pipefail, or a failed step mid-pipeline reads "
        "as success",
    )
    result = subprocess.run(["bash", "-n", str(script)], capture_output=True, text=True)
    check(
        result.returncode == 0,
        f"{script.name} does not parse as bash: {result.stderr}",
    )


def installer_dry_run():
    return run(INSTALLER, ["--dry-run"])


def check_preflight_faults() -> None:
    """On a host that cannot run a sweep, the installer must say why."""
    result = installer_dry_run()
    if sys.platform != "darwin":
        return
    check(
        "no /etc/os-release" in result.stdout,
        "on a non-Linux host the installer must report a missing "
        f"/etc/os-release; got:\n{result.stdout[:400]}",
    )
    # The message must name the architecture *this* machine reports, read here
    # rather than accepted in either spelling: an installer that prints
    # `aarch64` unconditionally is wrong on an arm64 Mac, which is where a dry
    # run is most often used, and a check that allows both words cannot see it.
    arch = subprocess.run(
        ["uname", "-m"], capture_output=True, text=True
    ).stdout.strip()
    check(
        f"{arch}: the recorded arms ran x86_64" in result.stdout,
        f"the architecture fault must name `{arch}`, the value uname reports; "
        f"got:\n{result.stdout[:400]}",
    )
    check(
        result.returncode != 0,
        "a dry run that found faults must exit non-zero, or it cannot be used "
        "as a pre-rental check",
    )


def check_installer_paths() -> None:
    """The dry run must walk every section, not stop at the first fault.

    On a Mac the preflight finds faults; a version that exited there would
    leave the Docker install, the Harbor pin and the image check unexecuted —
    precisely the untested-branch situation this file exists to prevent.
    """
    result = installer_dry_run()
    out = result.stdout
    for section in ("== preflight", "== packages", "== docker", "== repository",
                    "== harbor", "== checks", "== images", "== left for you"):
        check(section in out, f"the dry run never reached `{section}`")

    check(
        "would run: apt-get install" in out,
        "the package list must be visible in the dry run; it is the one thing "
        "a wrong host silently changes",
    )
    for package in ("build-essential", "pkg-config", "python3-venv", "xz-utils"):
        check(
            package in out,
            f"`{package}` is in the install list because a working-tree build "
            "needs it; it went missing from the dry run",
        )
    check(
        "harbor[modal]==0.27.0" in out,
        "harbor must be pinned: the adapter was written against 0.27.0's "
        "models, and an unpinned install drifts into a different harness",
    )
    # A dry run has to show the fresh-host path even when the machine running
    # it already has docker. Otherwise the only commands a reviewer ever sees
    # from the docker step are on a host that skips it, which is how the
    # Ubuntu-repo trap would get past review.
    for fragment in (
        "download.docker.com/linux/ubuntu/gpg",
        "docker-ce-cli",
        "docker-compose-plugin",
    ):
        check(
            fragment in out,
            f"the dry run never showed `{fragment}`; the docker install is the "
            "step whose Ubuntu-package alternative silently produces a CLI "
            "whose build lacks `--file`",
        )
    check(
        "dpkg --print-architecture" in INSTALLER.read_text(encoding="utf-8"),
        "the Docker apt source must use dpkg's architecture, not uname's: they "
        "agree on x86_64/amd64 by accident and differ on arm",
    )
    # The unsupported-arch fault can only be observed on a non-x86_64 host, so
    # on the machine CI runs this cannot fire. The source assertion can: an
    # interpolated `$arch` is what makes the message true wherever it prints.
    check(
        'problem "$arch: the recorded arms' in INSTALLER.read_text(encoding="utf-8"),
        "the architecture fault must interpolate `$arch`, not name one "
        "architecture: on the x86_64 CI box this line never executes, and the "
        "wrong one is only visible on the arm host a reviewer may be using",
    )
    check(
        "harborframework/terminal-bench:" in out
        and "-environment-v4.1.0" in out,
        "the image check must pull a real `harborframework/*` task image, not "
        "`alpine`: a registry mirror that refuses those digests serves alpine "
        "fine and fails every trial",
    )
    # The tag must be derived from a task list, not invented. An earlier
    # version pulled `...:hello-world`, which does not exist: the failure read
    # exactly like a mirror refusing digests, so the check reported a registry
    # problem on hosts that pull real images fine.
    check(
        "tasks-clean10.txt" in out,
        "the image tag must come from a real task list; a hardcoded tag that "
        "does not exist is indistinguishable from a registry failure",
    )
    check(
        "docker manifest inspect" in INSTALLER.read_text(encoding="utf-8"),
        "the remedy must say how to tell a missing tag from a refused digest; "
        "it only prints on failure, so this reads the source",
    )
    check(
        "measure-host-speed.sh" in out,
        "the closing instructions must point at the speed probe; every cost "
        "estimate depends on it",
    )
    # The instructions are the only place the venv's harbor is mentioned, and
    # `harbor` is a console script inside the venv: not on PATH, so the runner
    # dies with "harbor not found" after the whole install succeeded.
    check(
        "export PATH=" in out and "/bin:$PATH" in out,
        "the closing instructions must put the venv's harbor on PATH",
    )
    # A relative task list is resolved against the caller's directory, and the
    # suggested command is not run from the script's directory.
    check(
        re.search(r"codape /\S*/tasks-clean10\.txt", out) is not None,
        "the closing command must name the task list by absolute path",
    )
    # The adapter's DEFAULT_VERSION is v0.2.2; omitting --version measures the
    # previous release with no error at all.
    check(
        "--version v0.2.3" in out,
        "the closing command must pin --version explicitly",
    )
    check(
        "DEFAULT_VERSION is still" in out,
        "and must say why: the adapter's default is an older release",
    )


def check_version_gate() -> None:
    """The gates that refuse a too-old docker or python must read a version."""
    text = INSTALLER.read_text(encoding="utf-8")
    check(
        re.search(r"MIN_DOCKER_MAJOR=\d+", text) is not None,
        "the minimum Docker major version must be a named constant",
    )
    check(
        "unknown flag: --file" in text,
        "the version gate must say what an old CLI actually does, or the next "
        "person raises it for no reason",
    )
    check(
        "cut -d. -f1" in text,
        "the version gate must compare a major version; `docker --version` "
        "prints 'Docker version 29.4.0, build x', which does not compare",
    )
    # harbor 0.27.0 declares `requires_python >=3.12`; on 3.10 pip refuses it
    # with an error that names neither the interpreter nor the fix. A note is
    # not enough — the installer has to produce a 3.12 interpreter, or stop.
    check(
        "3, 12" in text,
        "the Python floor must be compared as a version, not assumed from the "
        "distro: 24.04 ships 3.12 and 22.04 ships 3.10",
    )
    check(
        "uv python install 3.12" in text and "deadsnakes" in text,
        "both ways to get 3.12 must be present: uv touches no apt source, "
        "deadsnakes is the fallback when uv is absent",
    )
    check(
        'run "${PYTHON:-python3}" -m venv' in text,
        "the venv must be built from the interpreter this script selected, or "
        "the 3.12 fallback above is decorative",
    )


def check_no_credential_handling() -> None:
    """Neither script may read or export a credential.

    The installer runs on a rented machine before any key exists, and the speed
    script only does GETs. A script that opened the key file would need its
    contents on a command line to use them, which is the failure the runner's
    own checks exist for.
    """
    for script in (INSTALLER, SPEED):
        text = script.read_text(encoding="utf-8")
        for name in ("CODAPE_AUTH_TOKEN", "CODAPE_API_KEY", "ANTHROPIC_AUTH_TOKEN"):
            check(
                f'{name}="' not in text and f"${'{'}{name}" not in text,
                f"{script.name} assigns {name}; credentials belong in the "
                "operator's key file, read by run-harbor.sh's own shell",
            )
            check(
                f"export {name}" not in text,
                f"{script.name} exports {name}",
            )


def check_speed_dry_run() -> None:
    result = run(SPEED, ["--dry-run"])
    check(result.returncode == 0, f"the speed dry run failed: {result.stderr}")
    check(
        "releases/download" in result.stdout,
        "the speed probe must measure the release asset every trial fetches, "
        "not a generic speed-test URL",
    )
    check(
        "not measured (no --upload-url)" in result.stdout,
        "upload must be reported as not measured unless a target is given: a "
        "POST publishes whatever it sends, so it cannot be the default",
    )
    text = SPEED.read_text(encoding="utf-8")
    check(
        "--retry-all-errors" in text,
        "the probe needs the same retry policy as the adapter: a truncated "
        "HTTP/2 stream looks like a fast, partial download and reads as a "
        "slow link",
    )
    check(
        "single-stream" in text and "parallel" in text,
        "both a single-stream and a parallel measurement are needed: the "
        "adapter's download is one curl, while docker pulls layers "
        "concurrently, and on a per-connection cap the two differ several-fold",
    )


def check_speed_verdict() -> None:
    """Feed a known rate through the script and read what it concludes.

    The verdict is the part a person acts on, so it is exercised rather than
    eyeballed. A stand-in `curl` on PATH reports a chosen rate, which is more
    faithful than re-implementing the arithmetic: it also tests the plumbing
    between curl's `-w` report and the sentence printed.
    """
    with tempfile.TemporaryDirectory() as raw:
        tmp = Path(raw)
        fake = tmp / "curl"
        fake.write_text(
            "#!/bin/sh\nprintf '%s' \"${FAKE_CURL_REPORT}\"\n", encoding="utf-8"
        )
        fake.chmod(0o755)
        env = {"PATH": f"{tmp}:{os.environ.get('PATH', '')}"}

        def probe(report: str):
            return subprocess.run(
                [str(SPEED), "--no-container"],
                capture_output=True,
                text=True,
                env={**os.environ, **env, "FAKE_CURL_REPORT": report},
            )

        # 5 KB/s: the 20 MB release needs ~4000 s, past the 2160 s budget.
        slow = probe("1000000 200.0 5000 200")
        check(
            "exceeds the setup budget" in slow.stdout,
            "a rate that cannot fetch the release inside 2160 s must be called "
            f"out; got:\n{slow.stdout}",
        )

        # 20 MB/s: the release lands in a second.
        fast = probe("1000000 0.05 20000000 200")
        check(
            "adequate for a paid sweep" in fast.stdout,
            f"a healthy link must be called adequate; got:\n{fast.stdout}",
        )


def check_json_output() -> None:
    with tempfile.TemporaryDirectory() as raw:
        out = Path(raw) / "speed.json"
        # A dry run measured nothing, so it must not write a report: all zeros
        # would be indistinguishable from a measured zero later.
        run(SPEED, ["--dry-run", "--json", str(out)])
        check(
            not out.exists(),
            "a --dry-run must not write --json; an all-zero report would read "
            "as a measured zero",
        )


def main() -> int:
    present = True
    for script in (INSTALLER, SPEED):
        if not script.is_file():
            check(False, f"{script.name} is missing")
            present = False
            continue
        check(os.access(script, os.X_OK), f"{script.name} is not executable")
        check_shell(script)

    if not present:
        for failure in FAILURES:
            print(f"error: {failure}", file=sys.stderr)
        return 1

    check_preflight_faults()
    check_installer_paths()
    check_version_gate()
    check_no_credential_handling()
    check_speed_dry_run()
    check_speed_verdict()
    check_json_output()

    if FAILURES:
        for failure in FAILURES:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print("host scripts: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
