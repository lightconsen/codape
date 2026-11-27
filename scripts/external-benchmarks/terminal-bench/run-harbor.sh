#!/usr/bin/env bash
# Run one Terminal-Bench 4.1 arm through Harbor.
#
# The repository could start a paid Codape run against the *old* harness
# (`run_terminal_bench.py`, terminal-bench 0.2.18) and nothing else. Every
# recorded 4.1 score came from a shell script on a host that no longer exists,
# which is why a one-letter typo in its local-binary variable survived an
# entire 108-trial sweep: a file outside the repository is a file nobody greps.
# This is that script, with what the sweep learned folded in.
#
# Usage:
#   run-harbor.sh <codape|claude> <task-list> [options] [-- extra harbor args]
#
# Examples:
#   run-harbor.sh codape tasks-control9.txt --version v0.2.3
#   run-harbor.sh codape tasks-clean10.txt --dry-run
#   run-harbor.sh codape tasks-all54.txt --concurrent 4 --version v0.2.3
#   RUN_HARBOR_KEY_FILE=~/codape-env.json run-harbor.sh claude tasks-control9.txt
#
# Credentials are read from a file by *this* shell and exported, never printed
# and never placed on a command line. The file is JSON: a flat object of name to
# value, with at least the three names in REQUIRED_KEY below. Keep it mode 0600.
# It is read with `python3 -I` so a planted `json.py` in the working directory
# cannot be imported in its place.
#
# Options:
#   -n, --concurrent N   Concurrent trials (default `auto` = this host's CPU
#                        count; each trial holds at least one container, and a
#                        task may declare a whole stack)
#       --resources P    `auto` (default) or `ignore`. `auto` leaves Docker's
#                        cgroup limits live; `ignore` removes them, and a task
#                        declaring more than the host has will then thrash it
#       --attempts N     Attempts per task (default 1)
#       --version TAG    Release tag to download, e.g. v0.2.3. Codape arm only;
#                        default comes from the adapter's DEFAULT_VERSION
#       --binary PATH    Upload this local executable instead of downloading a
#                        release. The adapter then asks the container which
#                        version it is, and records the host path in the trial
#       --timeout SEC    Adapter-imposed wall clock; empty (default) imposes none
#       --max-turns N    Codape --max-turns (default: adapter default, 300)
#       --pricing FILE   JSON price table uploaded into the config home, so the
#                        result frame carries total_cost_usd
#       --dry-run        Print the command that would run, then exit
#   -h, --help           This text
#
# The Codape arm refuses to start unless the configured model is in
# ALLOWED_MODELS. The gateway serves a fixed list, and a model outside it is
# rejected mid-trial with Generation.ModelNotInGo, which costs a container and
# twenty minutes to discover.

set -euo pipefail

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

# Harbor's CLI takes `--agent module:Class`, and the module must be importable.
# The adapter lives beside this script, so the directory that holds it is what
# goes on PYTHONPATH.
ADAPTER_DIR=$SCRIPT_DIR
ADAPTER=codape_harbor_agent:CodapeHarborAgent
CLAUDE_ADAPTER=claude-code
DATASET=terminal-bench/terminal-bench@4.1.0

# The gateway answers for these and gets a hard failure for anything else.
# `deepseek-v4.1-flash` is what every recorded 4.1 trial used; add a line here
# when the gateway gains a model, rather than loosening the check.
ALLOWED_MODELS="deepseek-v4.1-flash"

REQUIRED_KEY="CODAPE_AUTH_TOKEN CODAPE_BASE_URL CODAPE_MODEL"

usage() {
    sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p;}' "${BASH_SOURCE[0]}"
    exit "${1:-0}"
}

die() {
    echo "error: $*" >&2
    exit 1
}

# --- arguments -------------------------------------------------------------

ARM=""
TASK_LIST=""
# Empty means `auto`, resolved once the host is known. `n` is an experimental
# parameter and not a speed knob: the same nine tasks on the same host at `-n 2`
# and `-n 4` flip two results (`vf2-speedup-networkx` 0->1, `kv-live-surgery`
# 1->0) and move one task's input by 22x, because `--resources auto` gives each
# container a share that concurrency decides. So the value is resolved, printed,
# and left in the job's own `config.json` rather than being implied.
CONCURRENT=""
ATTEMPTS=1
RESOURCES="auto"
VERSION=""
BINARY=""
TIMEOUT=""
MAX_TURNS=""
PRICING=""
DRY_RUN=0
EXTRA=()

need_value() {
    [ "$#" -ge 2 ] && [ -n "$2" ] || die "$1 needs a value"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help) usage 0 ;;
        -n|--concurrent) need_value "$@"; CONCURRENT=$2; shift 2 ;;
        --resources) need_value "$@"; RESOURCES=$2; shift 2 ;;
        --attempts) need_value "$@"; ATTEMPTS=$2; shift 2 ;;
        --version) need_value "$@"; VERSION=$2; shift 2 ;;
        --binary) need_value "$@"; BINARY=$2; shift 2 ;;
        --timeout) need_value "$@"; TIMEOUT=$2; shift 2 ;;
        --max-turns) need_value "$@"; MAX_TURNS=$2; shift 2 ;;
        --pricing) need_value "$@"; PRICING=$2; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        --) shift; EXTRA=("$@"); break ;;
        -*) die "unknown option: $1 (try --help)" ;;
        *)
            if [ -z "$ARM" ]; then
                ARM=$1
            elif [ -z "$TASK_LIST" ]; then
                TASK_LIST=$1
            else
                die "unexpected argument: $1"
            fi
            shift
            ;;
    esac
done

[ -n "$ARM" ] || die "missing arm (codape or claude); try --help"
[ -n "$TASK_LIST" ] || die "missing task list; try --help"

case "$ARM" in
    codape|claude) ;;
    *) die "unknown arm: $ARM (expected codape or claude)" ;;
esac

[ -r "$TASK_LIST" ] || die "no task list at $TASK_LIST"
[ -s "$TASK_LIST" ] || die "task list $TASK_LIST is empty"

# Harbor's `-i` is repeatable and each value is a literal, so the argv is built
# from the file line by line rather than by word-splitting one string.
TASK_ARGS=()
while IFS= read -r name; do
    # Trim surrounding whitespace and skip blanks and comments, so a list can
    # carry a note explaining why a task is in it.
    name=${name#"${name%%[![:space:]]*}"}
    name=${name%"${name##*[![:space:]]}"}
    [ -n "$name" ] || continue
    case "$name" in \#*) continue ;; esac
    TASK_ARGS+=( -i "terminal-bench/$name" )
done < "$TASK_LIST"

[ "${#TASK_ARGS[@]}" -gt 0 ] || die "task list $TASK_LIST has no task names"

# --- concurrency and resource policy ---------------------------------------

# `auto` is the host's CPU count, with no cap. That is not a guess: it reproduces
# both recorded baselines exactly — 2 on Part 1's 2-vCPU host, 4 on Part 2's and
# Part 3's 4-vCPU host — and it gives a larger host the trials it can hold. Any
# other formula (a cap of 4, a memory-derived share) yields a number no recorded
# run used, and the numbers above show `n` moves results, not just the clock.
#
# `getconf` rather than `nproc`: it exists on macOS and Ubuntu alike, and `nproc`
# does not exist on macOS at all. Under a cgroup limit it reports the machine's
# count rather than the container's, so a run inside one passes `-n` explicitly.
cpu_count() {
    local n
    n=$(getconf _NPROCESSORS_ONLN 2>/dev/null) || n=""
    case "$n" in ''|*[!0-9]*) n=0 ;; esac
    if [ "$n" -lt 1 ] && [ "$(uname -s 2>/dev/null)" = Darwin ]; then
        n=$(sysctl -n hw.ncpu 2>/dev/null) || n=""
        case "$n" in ''|*[!0-9]*) n=0 ;; esac
    fi
    printf '%s\n' "$n"
}

if [ -z "$CONCURRENT" ]; then
    CONCURRENT=$(cpu_count)
    [ "$CONCURRENT" -ge 1 ] ||
        die "cannot detect this host's CPU count; pass --concurrent N"
    echo "note: --concurrent defaults to auto = $CONCURRENT on this host" >&2
fi
case "$CONCURRENT" in
    ''|*[!0-9]*) die "--concurrent needs a positive integer, got '$CONCURRENT'" ;;
esac
[ "$CONCURRENT" -ge 1 ] || die "--concurrent needs a positive integer"

case "$RESOURCES" in
    auto|ignore) ;;
    *) die "--resources is auto or ignore, not '$RESOURCES'" ;;
esac

# --- arm-specific constraints ---------------------------------------------

if [ "$ARM" = claude ]; then
    # Claude Code takes a tarball from Harbor's own agent, so neither pin
    # applies to it. Refuse them rather than silently ignoring them: a run that
    # was supposed to measure a local binary and measured a release instead is
    # the exact defect this script exists to prevent.
    [ -z "$VERSION" ] || die "--version applies to the codape arm only"
    [ -z "$BINARY" ] || die "--binary applies to the codape arm only"
    [ -z "$PRICING" ] || die "--pricing applies to the codape arm only"
fi

if [ -n "$BINARY" ]; then
    # The adapter raises FileNotFoundError for this, but only after Harbor has
    # built the trial; failing here costs nothing.
    [ -f "$BINARY" ] || die "no binary at $BINARY"
    [ -x "$BINARY" ] || die "not executable: $BINARY"
    case "$BINARY" in
        /*) ;;
        *) BINARY=$(CDPATH='' cd -- "$(dirname -- "$BINARY")" && pwd)/$(basename -- "$BINARY") ;;
    esac
    if [ -n "$VERSION" ]; then
        # Dropped, not merely ignored. The adapter keeps an explicit version
        # beside `binary_path` — it reads that as the caller naming the build —
        # so passing both would stamp `agent_info.version` with a release the
        # trial did not run. That is the defect this whole path was fixed for.
        echo "note: ignoring --version $VERSION; with --binary the container is" \
            "asked for its own version" >&2
        VERSION=""
    fi
fi

if [ -n "$PRICING" ]; then
    [ -r "$PRICING" ] || die "no price table at $PRICING"
fi

# --- credentials -----------------------------------------------------------

# Read by this shell, into this shell. `-I` keeps the working directory off
# sys.path; `shlex.quote` means a value with a quote or a space in it is
# exported intact rather than broken or, worse, echoed.
read_key_file() {
    python3 -I - "$1" "$REQUIRED_KEY" <<'PY'
import json, shlex, sys

path, required = sys.argv[1], sys.argv[2].split()
try:
    with open(path, encoding="utf-8") as handle:
        values = json.load(handle)
except OSError as exc:
    raise SystemExit(f"error: cannot read {path}: {exc}")
except json.JSONDecodeError as exc:
    raise SystemExit(f"error: {path} is not JSON: {exc}")
if not isinstance(values, dict):
    raise SystemExit(f"error: {path} must be a JSON object")
missing = [name for name in required if not values.get(name)]
if missing:
    # Names only. The file is checked by shape; no value is printed.
    raise SystemExit(f"error: {path} is missing: {', '.join(missing)}")
for name, value in values.items():
    print(f"export {name}={shlex.quote(str(value))}")
PY
}

KEY_FILE=${RUN_HARBOR_KEY_FILE:-$HOME/codape-env.json}

if [ "$DRY_RUN" -eq 0 ]; then
    [ -r "$KEY_FILE" ] || die "no credentials at $KEY_FILE; set RUN_HARBOR_KEY_FILE"
    eval "$(read_key_file "$KEY_FILE")"
    for name in $REQUIRED_KEY; do
        [ -n "${!name:-}" ] || die "$name is empty after reading $KEY_FILE"
    done
    case " $ALLOWED_MODELS " in
        *" $CODAPE_MODEL "*) ;;
        *)
            # Failing here is cheaper than finding out twenty minutes into a
            # paid trial. The sweep hit this as Generation.ModelNotInGo.
            die "CODAPE_MODEL is '$CODAPE_MODEL', which the gateway does not serve;" \
                "allowed: $ALLOWED_MODELS (see ALLOWED_MODELS in $0)"
            ;;
    esac
fi

# --- harbor ----------------------------------------------------------------

HARBOR=${RUN_HARBOR_BIN:-harbor}
command -v "$HARBOR" >/dev/null 2>&1 ||
    [ -x "$HARBOR" ] ||
    die "harbor not found (set RUN_HARBOR_BIN to its path)"

COMMAND=("$HARBOR" run -d "$DATASET")

if [ "$ARM" = codape ]; then
    # The images carry none of build-essential's prerequisites, and the agent's
    # setup on a slow link is what `--agent-setup-timeout-multiplier` exists for.
    # 6 is what every recorded 4.1 trial used; the Claude arm's curl install hit
    # the 360 s default in the sweep, so it gets the same multiplier.
    COMMAND+=( -a "$ADAPTER" --agent-setup-timeout-multiplier 6 )
    [ -z "$MAX_TURNS" ] || COMMAND+=( --agent-kwarg "max_turns=$MAX_TURNS" )
    [ -z "$VERSION" ] || COMMAND+=( --agent-kwarg "version=$VERSION" )
    [ -z "$BINARY" ] || COMMAND+=( --agent-kwarg "binary_path=$BINARY" )
    [ -z "$TIMEOUT" ] || COMMAND+=( --agent-kwarg "timeout_sec=$TIMEOUT" )
    [ -z "$PRICING" ] || COMMAND+=( --agent-kwarg "pricing_path=$PRICING" )
else
    COMMAND+=( -a "$CLAUDE_ADAPTER" --agent-setup-timeout-multiplier 6 )
    [ -z "$MAX_TURNS" ] || COMMAND+=( --agent-kwarg "max_turns=$MAX_TURNS" )
    # Claude Code reads ANTHROPIC_*, not CODAPE_*. The adapter's model-connection
    # spec does not apply to a built-in agent, so these are passed explicitly.
    # A real run has already refused an empty value above; the fallbacks only
    # exist so `--dry-run` can print the shape without reading the key file.
    COMMAND+=( --model "${CODAPE_MODEL:-<CODAPE_MODEL>}"
               --base-url "${CODAPE_BASE_URL:-<CODAPE_BASE_URL>}"
               --api-key-name ANTHROPIC_AUTH_TOKEN )
fi

# The policy: `auto` is Harbor's own default, so it is spelled by *omitting* the
# flags — which is exactly what the 54-task baseline did, and its `config.json`
# is the evidence that omitting them means `auto` (it records
# `cpu_enforcement_policy: "auto"`). Writing `--cpus auto` instead would assume
# the CLI accepts its own default as a literal; nothing in the archive shows
# that, and a rejected value there is a paid run that never starts. `ignore` is
# passed because it is the opt-in, and it is the policy Part 1 and the Claude
# control arm ran under: without it, a task declaring more than the host has is
# refused rather than allowed to thrash it (Part 1 let a 16-CPU task take 144.5
# minutes to reach 0/1 on a 2-vCPU host).
if [ "$RESOURCES" = ignore ]; then
    COMMAND+=( --cpus ignore --memory ignore )
else
    # Named so `--dry-run` and the log say the policy out loud, without putting
    # a value on Harbor's command line that Harbor may not parse.
    echo "note: --resources auto; Docker's cgroup limits stay live (Harbor's default)" >&2
fi

COMMAND+=( -n "$CONCURRENT" )
[ "$ATTEMPTS" = 1 ] || COMMAND+=( --n-attempts "$ATTEMPTS" )
COMMAND+=( "${TASK_ARGS[@]}" )
[ "${#EXTRA[@]}" -eq 0 ] || COMMAND+=( "${EXTRA[@]}" )

# The adapter is imported by name, so the directory holding it has to be on
# PYTHONPATH. Prepending rather than replacing keeps a caller's own additions.
export PYTHONPATH="$ADAPTER_DIR${PYTHONPATH:+:$PYTHONPATH}"

if [ "$DRY_RUN" -eq 1 ]; then
    # The command only. The environment a real run builds is not printed,
    # because it contains the token.
    echo "PYTHONPATH=$ADAPTER_DIR"
    printf '%q ' "${COMMAND[@]}"
    printf '\n'
    exit 0
fi

if [ "$ARM" = claude ]; then
    export ANTHROPIC_AUTH_TOKEN=$CODAPE_AUTH_TOKEN
    export ANTHROPIC_BASE_URL=$CODAPE_BASE_URL
    export ANTHROPIC_MODEL=$CODAPE_MODEL
    export ANTHROPIC_CUSTOM_HEADERS=${CODAPE_CUSTOM_HEADERS:-}
fi

# `exec` so the trial's exit status is this script's, and so signals reach
# Harbor rather than stopping at a wrapper.
exec "${COMMAND[@]}"
