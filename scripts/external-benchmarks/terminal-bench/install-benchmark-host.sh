#!/usr/bin/env bash
# Install a fresh Ubuntu host to run Terminal-Bench 4.1 arms.
#
# Everything here is derived from what the recorded sweeps actually needed, and
# every step that could fail silently is checked afterwards. Nothing is
# downloaded from a URL this repository does not name, nothing is written into
# a shell profile, and no credential is read or written: the key file is the
# operator's to place, and this script only says where it goes and what mode it
# needs.
#
# What it installs, and why each is here rather than assumed:
#   docker + compose plugin   Harbor drives every trial through `docker compose`
#   python 3.12+              harbor 0.27 requires it; Ubuntu 22.04 ships 3.10
#   harbor==0.27.0            pinned: the adapter was written and checked
#                             against this version's models and abstract bases
#   git, curl, jq, rsync      fetch the repo; jq for inspecting result.json
#   build-essential, pkg-config
#                             `x86_64-unknown-linux-musl` cross-builds need
#                             them when the arm runs from a working tree
#
# The two host-level traps this encodes, both of which cost a sweep:
#   - Docker's registry path matters. A mirror can refuse `harborframework/*`
#     digests, which is every 4.1 task image. The script checks that the exact
#     image family pulls, not just that `docker pull alpine` works.
#   - The `docker` CLI on Ubuntu 20.04's repo is 20.10, whose `build` had no
#     `--file`; Harbor passes it. A trial on such a host dies with
#     `unknown flag: --file` before the agent starts, which reads as an agent
#     that never ran. The check refuses a CLI older than 24.
#
# Usage:
#   install-benchmark-host.sh [--repo-url URL] [--ref REF] [--skip-docker]
#                             [--skip-images] [--dry-run]
#
# Not idempotent by accident: every step reports whether it was already
# satisfied. Re-running is safe.

set -euo pipefail

REPO_URL=${REPO_URL:-https://github.com/lightconsen/codape.git}
REPO_REF=${REPO_REF:-main}
DEST=${DEST:-$HOME/codape}
VENV=${VENV:-$HOME/harbor-venv}
HARBOR_VERSION=0.27.0
MIN_DOCKER_MAJOR=24

SKIP_DOCKER=0
SKIP_IMAGES=0
DRY_RUN=0

usage() {
    sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p;}' "${BASH_SOURCE[0]}"
    exit "${1:-0}"
}
die() { echo "error: $*" >&2; exit 1; }
say() { printf '\n== %s\n' "$*"; }
ok() { printf '   ok: %s\n' "$*"; }
note() { printf '   note: %s\n' "$*"; }

# A condition that makes a real run impossible, but should not stop a dry run:
# a dry run on the wrong machine is how you check the script before renting
# the right one, so it reports the fault and keeps walking instead of exiting
# at the first line. The exit status of a dry run still reflects it.
PROBLEMS=0
problem() {
    PROBLEMS=$((PROBLEMS + 1))
    if [ "$DRY_RUN" -eq 1 ]; then
        printf '   PROBLEM: %s\n' "$*"
        return 0
    fi
    die "$*"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help) usage 0 ;;
        --repo-url) [ "$#" -ge 2 ] || die "--repo-url needs a value"; REPO_URL=$2; shift 2 ;;
        --ref) [ "$#" -ge 2 ] || die "--ref needs a value"; REPO_REF=$2; shift 2 ;;
        --dest) [ "$#" -ge 2 ] || die "--dest needs a value"; DEST=$2; shift 2 ;;
        --skip-docker) SKIP_DOCKER=1; shift ;;
        --skip-images) SKIP_IMAGES=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

# Root is needed for apt and the docker install. Rather than re-exec with sudo
# implicitly, say so and stop: a script that silently becomes root is a script
# whose side effects nobody reviews. A dry run needs no privileges — it is most
# useful on a machine that is not the target.
SUDO=""
if [ "$DRY_RUN" -eq 0 ]; then
    if [ "$(id -u)" -ne 0 ]; then
        sudo -n true 2>/dev/null ||
            die "this needs root (or passwordless sudo) for apt and the docker install"
        SUDO="sudo"
    fi
fi


run() {
    if [ "$DRY_RUN" -eq 1 ]; then
        printf '   would run: %s\n' "$*"
        return 0
    fi
    "$@"
}

# --- preflight --------------------------------------------------------------

say "preflight"
# Everything that follows is read from these; on a non-Linux host they are
# absent, and a dry run still needs plausible values to walk through.
if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
    case "${ID:-}" in
        ubuntu|debian) ok "$PRETTY_NAME" ;;
        *) problem "$PRETTY_NAME is not Ubuntu or Debian; the package names below may differ" ;;
    esac
else
    PRETTY_NAME=$(uname -s)
    problem "no /etc/os-release; this is written for Ubuntu (found $PRETTY_NAME)"
fi
arch=$(uname -m)
case "$arch" in
    x86_64|amd64) ok "x86_64" ;;
    aarch64|arm64)
        # The release asset is x86_64-unknown-linux-musl, and every 4.1 task
        # image is amd64, so a run here needs emulation and will be slow enough
        # that the numbers are not comparable to the recorded ones.
        problem "$arch: the recorded arms ran x86_64 binaries and images;
                use an x86_64 host or expect results that are not comparable" ;;
    *) problem "unsupported architecture: $arch" ;;
esac
cpus=$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo "?")
if [ -r /proc/meminfo ]; then
    mem_kb=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
else
    mem_kb=$(python3 -c 'print(int(__import__("os").sysconf("SC_PAGE_SIZE") * __import__("os").sysconf("SC_PHYS_PAGES") / 1024))' 2>/dev/null || echo 0)
fi
mem_gb=$(python3 -c "print(round(${mem_kb:-0}/1048576, 1))" 2>/dev/null || echo "?")
note "host: ${cpus} CPUs, ${mem_gb} GB RAM"
# `-n` defaults to the CPU count, so this is the concurrency a run will choose.
# Worth saying out loud before a 16-hour sweep discovers it.
note "the runner will default to -n ${cpus}; each trial holds at least one container"
# GNU `df -P` is portable enough, but `-B`/`-G` are not — on Darwin `df -PG`
# exits 64, and under `set -e` a failing command substitution in an assignment
# ends the script. That is invisible on the Ubuntu host this is for, so the
# fallback is here rather than discovered there.
avail=$(df -Pk / 2>/dev/null | tail -1 | awk '{printf "%d", $4/1048576}') || avail=""
if [ -n "${avail:-}" ] && [ "${avail:-0}" -lt 40 ] 2>/dev/null; then
    note "only ${avail} GB free on /; a 54-task sweep pulls several task images"
else
    ok "${avail:-?} GB free on /"
fi
if [ "$DRY_RUN" -eq 1 ]; then
    ok "dry run: this host is not required to be Ubuntu"
fi

# --- packages ---------------------------------------------------------------

say "packages"
run $SUDO apt-get update -qq
run $SUDO apt-get install -y -qq \
    ca-certificates curl git jq rsync xz-utils unzip \
    python3 python3-venv python3-pip \
    build-essential pkg-config
# harbor 0.27.0 declares `requires_python >= 3.12`, and pip on 3.10 refuses to
# install it at all. Ubuntu 24.04 ships 3.12 and passes this; 22.04 ships 3.10
# and does not, and is handled by `ensure_python312` below rather than by a note
# nobody reads — a warning here would let the venv step fail with a pip error
# that names neither the version nor the fix.
if [ "$DRY_RUN" -eq 0 ]; then
    python3 -c 'import sys; raise SystemExit(0 if sys.version_info >= (3, 12) else 1)' ||
        note "ubuntu's python3 is $(python3 -V 2>&1); see the 3.12 install below"
fi

# --- docker -----------------------------------------------------------------

if [ "$SKIP_DOCKER" -eq 1 ]; then
    say "docker: skipped by request"
else
    say "docker"
    # In a dry run the install commands are shown even when this host already
    # has docker: the branch that matters is the one a *fresh* host takes, and
    # a dry run on a machine that is not the target is the only place it is
    # reviewed before it runs for real.
    installed=0
    if command -v docker >/dev/null 2>&1; then
        installed=1
        ok "already installed: $(docker --version)"
    fi
    if [ "$installed" -eq 0 ] || [ "$DRY_RUN" -eq 1 ]; then
        # Docker's own repository, not Ubuntu's: Ubuntu 20.04's `docker.io` is
        # 20.10 and its `build` has no `--file`, which Harbor passes on every
        # trial. That failure reads as an agent that never started.
        run $SUDO install -m 0755 -d /etc/apt/keyrings
        run bash -c "$SUDO curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
            -o /etc/apt/keyrings/docker.asc"
        run bash -c "$SUDO chmod a+r /etc/apt/keyrings/docker.asc"
        run bash -c ". /etc/os-release && printf 'deb [arch=%s signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu %s stable\n' \
            \"\$(dpkg --print-architecture)\" \"\$VERSION_CODENAME\" | \
            $SUDO tee /etc/apt/sources.list.d/docker.list >/dev/null"
        run $SUDO apt-get update -qq
        run $SUDO apt-get install -y -qq docker-ce docker-ce-cli containerd.io \
            docker-buildx-plugin docker-compose-plugin
    fi
    if [ "$DRY_RUN" -eq 0 ]; then
        # The CLI version is the one that matters, not the daemon's: `--file`
        # is a buildx/CLI flag.
        version=$(docker --version | awk '{print $3}' | cut -d. -f1)
        [ "${version:-0}" -ge "$MIN_DOCKER_MAJOR" ] ||
            problem "docker CLI is major ${version}; Harbor's build needs
                     --file, which is absent before ${MIN_DOCKER_MAJOR}. A
                     trial here fails with 'unknown flag: --file' before the
                     agent starts."
        ok "docker CLI major ${version} >= ${MIN_DOCKER_MAJOR}"
        $SUDO usermod -aG docker "$(id -un)" 2>/dev/null || true
        note "added $(id -un) to the docker group; it takes effect in a new login
          shell. Until then run this script's checks under sudo."
        docker compose version >/dev/null 2>&1 ||
            problem "docker compose plugin missing; Harbor drives every trial with it"
        ok "docker compose: $(docker compose version --short 2>/dev/null || echo present)"
        docker info >/dev/null 2>&1 ||
            note "the daemon is not reachable as $(id -un) yet (group membership)"
    fi
fi

# --- the repository ---------------------------------------------------------

say "repository"
if [ -d "$DEST/.git" ]; then
    ok "already at $DEST"
    run git -C "$DEST" fetch -q origin
    run git -C "$DEST" checkout -q "$REPO_REF"
    run git -C "$DEST" pull -q --ff-only origin "$REPO_REF" || true
else
    run git clone -q --branch "$REPO_REF" "$REPO_URL" "$DEST"
fi
if [ "$DRY_RUN" -eq 0 ]; then
    ok "$(git -C "$DEST" log --oneline -1)"
    [ -f "$DEST/scripts/external-benchmarks/terminal-bench/run-harbor.sh" ] ||
        note "the runner is missing from this ref; it landed in 8a8d365c"
fi

# --- harbor -----------------------------------------------------------------

say "harbor $HARBOR_VERSION"

# harbor 0.27.0's `requires_python` is >=3.12, and pip refuses to install it on
# anything older with an error that names neither the interpreter it used nor
# the fix. Ubuntu 24.04 ships 3.12; 22.04 ships 3.10. `uv` is used when it can
# supply an interpreter, because that is one command and touches no apt source;
# deadsnakes is the fallback and the one that writes into /etc. The interpreter
# chosen here is the one the venv is built from, so this decides the run.
PYTHON=${PYTHON:-}
if [ -z "$PYTHON" ]; then
    PYTHON=$(command -v python3.12 2>/dev/null || true)
fi
if [ -z "$PYTHON" ]; then
    PYTHON=$(command -v python3 2>/dev/null || true)
    if [ -n "$PYTHON" ] &&
       ! "$PYTHON" -c 'import sys; raise SystemExit(0 if sys.version_info >= (3, 12) else 1)' 2>/dev/null; then
        PYTHON=""
    fi
fi
if [ -z "$PYTHON" ]; then
    say "python 3.12 (harbor requires it)"
    if command -v uv >/dev/null 2>&1; then
        run uv python install 3.12
        PYTHON=$(uv python find 3.12 2>/dev/null || true)
    else
        run bash -c "$SUDO add-apt-repository -y ppa:deadsnakes/ppa && \
            $SUDO apt-get update -qq && \
            $SUDO apt-get install -y -qq python3.12 python3.12-venv"
        PYTHON=$(command -v python3.12 2>/dev/null || true)
    fi
    if [ "$DRY_RUN" -eq 0 ] && [ -z "$PYTHON" ]; then
        die "no Python 3.12 after installing one; harbor $HARBOR_VERSION requires
             >=3.12 and pip will refuse it on anything older"
    fi
    ok "python for the venv: ${PYTHON:-a 3.12 interpreter}"
fi
[ "$DRY_RUN" -eq 1 ] || ok "python for the venv: $($PYTHON -V 2>&1)"

if [ -x "$VENV/bin/harbor" ]; then
    have=$("$VENV/bin/harbor" --version 2>/dev/null | awk '{print $NF}' || echo "?")
    ok "already installed in $VENV (version ${have})"
    [ "$have" = "$HARBOR_VERSION" ] ||
        note "the adapter was checked against $HARBOR_VERSION; $have is a
          different harness and its trial layout may differ"
else
    run "${PYTHON:-python3}" -m venv "$VENV"
    run "$VENV/bin/pip" install -q --upgrade pip
    run "$VENV/bin/pip" install -q "harbor[modal]==$HARBOR_VERSION"
    if [ "$DRY_RUN" -eq 0 ]; then
        "$VENV/bin/harbor" --version >/dev/null 2>&1 ||
            die "harbor did not install; check $VENV/bin/pip install harbor==$HARBOR_VERSION"
        ok "$("$VENV/bin/harbor" --version 2>/dev/null || echo installed)"
    fi
fi

# --- the checks that catch a wrong host -------------------------------------

say "checks"
if [ "$DRY_RUN" -eq 0 ]; then
    PYTHONPATH="$DEST/scripts/external-benchmarks/terminal-bench" \
        "$VENV/bin/python" -I \
        "$DEST/scripts/external-benchmarks/check_harbor_agent.py" ||
        problem "the adapter check failed; fix it before spending anything"

    # The runner's own check needs no harbor and no Docker, so it is the fastest
    # thing that says the script is intact.
    python3 -I "$DEST/scripts/external-benchmarks/check_harbor_runner.py" ||
        problem "the runner check failed"
    ok "adapter and runner checks pass"
fi

# --- images -----------------------------------------------------------------

if [ "$SKIP_IMAGES" -eq 1 ]; then
    say "images: skipped by request"
else
    say "images"
    # Pulling one 4.1 task image is the only cheap way to prove that the
    # registry this host can reach actually serves `harborframework/*`. A
    # mirror that refuses those digests lets `docker pull alpine` succeed and
    # every trial fail, which is a very expensive way to learn it.
    #
    # The tag comes from the task list rather than being invented. An earlier
    # version of this check pulled `harborframework/terminal-bench:hello-world`,
    # which is not a tag that exists: the repository carries per-task tags in
    # two forms, `<task>-environment-<contenthash>` and the stable
    # `<task>-environment-v4.1.0`. A missing tag and a mirror that refuses
    # digests fail the same way, so the invented name made every install report
    # a registry problem that was never there — including on hosts that pull
    # real images in six seconds.
    task=$(grep -v '^[[:space:]]*#' "$DEST/scripts/external-benchmarks/terminal-bench/tasks-clean10.txt" 2>/dev/null \
        | grep -v '^[[:space:]]*$' | head -1 | tr -d '[:space:]') || task=""
    [ -n "$task" ] || task="fin-saccr-rwa"
    image="harborframework/terminal-bench:${task}-environment-v4.1.0"
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "   would run: docker pull $image"
    else
        if timeout 900 $SUDO docker pull -q "$image" >/dev/null 2>&1; then
            ok "pulled $image"
        else
            cat >&2 <<MSG
   note: could not pull $image
         That is the family every 4.1 task image belongs to, and a registry
         mirror that refuses those digests is a documented failure here — it
         lets \`docker pull alpine\` succeed while every trial fails. But check
         the reference first: \`docker manifest inspect $image\` distinguishes a
         tag that does not exist from a mirror that will not serve it.
         Try \`docker pull mirror.gcr.io/$image\`
         then \`docker tag mirror.gcr.io/$image $image\`,
         or point the daemon at docker.io directly.
MSG
        fi
        # Docker Hub rate-limits anonymous pulls per IP. A sweep pulls dozens.
        note "anonymous pulls are rate-limited per IP; log in or mirror if a
          sweep stalls with 'toomanyrequests'"
    fi
fi

# --- what only the operator can do ------------------------------------------

say "left for you"
cat <<MSG
   1. Place the credential file at \$HOME/codape-env.json, mode 0600, as a flat
      JSON object with at least CODAPE_AUTH_TOKEN, CODAPE_BASE_URL and
      CODAPE_MODEL. The runner reads it in its own shell and never prints it:
          chmod 600 \$HOME/codape-env.json
      No value is needed by this script and none is written by it.

   2. Put the venv's harbor on PATH. Everything below needs it, and the error
      without it is just "harbor not found":
          export PATH="$VENV/bin:\$PATH"

   3. Measure the link before a paid run, because every cost and duration
      estimate depends on it and one recorded snapshot was pathological:
          $DEST/scripts/external-benchmarks/terminal-bench/measure-host-speed.sh \\
              --json speed.json

   4. Then the cheap stage first, and only after it matches the recorded
      numbers does the rest make sense. The task list is given by absolute
      path: the runner resolves it against the current directory, and that is
      not the directory holding this script.
          RUN_HARBOR_KEY_FILE=\$HOME/codape-env.json \\
          $DEST/scripts/external-benchmarks/terminal-bench/run-harbor.sh \\
              codape $DEST/scripts/external-benchmarks/terminal-bench/tasks-clean10.txt \\
              --version v0.2.3 --dry-run
      (drop --dry-run to spend; the dry run prints the full argv.)

      Pass --version explicitly. The adapter's own DEFAULT_VERSION is still
      v0.2.2, so a run that omits it measures the previous release silently.
MSG

# Non-zero after a dry run that found faults, so this is usable as a CI or
# pre-rental check rather than something to read and hope about.
if [ "$PROBLEMS" -gt 0 ]; then
    printf '\n%d problem(s) would stop a real run on this host.\n' "$PROBLEMS" >&2
    exit 1
fi
printf '\nready: %s\n' "$(git -C "$DEST" log --oneline -1 2>/dev/null || echo 'no checkout')"

