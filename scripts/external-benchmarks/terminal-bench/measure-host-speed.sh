#!/usr/bin/env bash
# Measure this host's network, because every cost and duration estimate for a
# sweep depends on it and one snapshot of it was pathological.
#
# The archived numbers were taken over a link where build-essential (95.7 MB)
# did not finish in 30 minutes and opencv (55 MB) never passed its 900 s line,
# while another host did 18.86 MB/s on the same tasks. A full 54-task sweep
# pulls a release archive into every trial's container and a task image per
# task, so throughput is not a detail — at 50 KB/s the sweep is not slow, it is
# impossible. This measures it in the two places it matters: the host, and
# inside a container, because the release download happens in the container.
#
# Nothing is uploaded unless you name a target with --upload-url. A GET sends
# no data; a POST to a third party would publish whatever you send, so the
# upload half is opt-in rather than assumed.
#
# Usage:
#   measure-host-speed.sh [options]
#
# Options:
#   --upload-url URL   Also measure upload by POSTing a small buffer here.
#                      Without it the upload half is reported as not measured —
#                      which is honest, and is not the same as "fine".
#   --no-container     Host only; skip the in-container measurement
#   --json FILE        Also write the raw measurements as JSON
#   --dry-run          Print what would be measured, then exit
#   -h, --help         This text
#
# Exit status is 0 when the measurements ran (whatever they found), and 1 when
# a measurement could not be taken for a reason other than the host being slow.

set -euo pipefail

# The artifact every Codape trial downloads into its container. Measuring this
# URL rather than some generic speed-test file is the point: it is the exact
# transfer that decides whether `--agent-setup-timeout-multiplier 6` is enough.
RELEASE_URL=${RELEASE_URL:-https://github.com/lightconsen/codape/releases/download/v0.2.3/codape-x86_64-unknown-linux-musl.tar.gz}
# About what that archive weighs; used to turn a rate into a duration. This is
# the measured size of v0.2.3's asset, not a round number — a plan that assumes
# 21 MB when the file is 20.0 MB is wrong in the safe direction here, but the
# point of measuring is not to guess a second time.
RELEASE_BYTES=${RELEASE_BYTES:-20011618}
# Harbor's default agent-setup budget is 360 s, and the runner passes x6.
SETUP_BUDGET_SEC=${SETUP_BUDGET_SEC:-2160}
# A task image is pulled per task; this is a planning figure, not a measurement.
IMAGE_BYTES=${IMAGE_BYTES:-500000000}

UPLOAD_URL=""
WITH_CONTAINER=1
JSON_OUT=""
DRY_RUN=0

usage() {
    sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p;}' "${BASH_SOURCE[0]}"
    exit "${1:-0}"
}

die() {
    echo "error: $*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help) usage 0 ;;
        --upload-url) [ "$#" -ge 2 ] || die "--upload-url needs a value"; UPLOAD_URL=$2; shift 2 ;;
        --no-container) WITH_CONTAINER=0; shift ;;
        --json) [ "$#" -ge 2 ] || die "--json needs a path"; JSON_OUT=$2; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

have() { command -v "$1" >/dev/null 2>&1; }

# --- what is being measured -------------------------------------------------

if [ "$DRY_RUN" -eq 1 ]; then
    echo "release URL:     $RELEASE_URL"
    echo "release bytes:   $RELEASE_BYTES"
    echo "setup budget:    $SETUP_BUDGET_SEC s"
    echo "container part:  $([ "$WITH_CONTAINER" -eq 1 ] && echo "yes (needs docker)" || echo "skipped")"
    echo "upload part:     ${UPLOAD_URL:-not measured (no --upload-url)}"
    echo
    echo "would measure: single-stream download, 4 parallel streams, connect"
    echo "latency, one container pull + one container download, and upload if"
    echo "a target was given."
    exit 0
fi

have curl || die "curl is required"
have python3 || die "python3 is required"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# curl's own report, in one line: bytes, seconds, speed in bytes/s. Taking the
# speed from curl rather than dividing by `date` keeps a DNS stall or a retry
# from being charged to the transfer.
#
# `--retry-all-errors` matters here for the same reason it matters in the
# adapter: a truncated HTTP/2 stream is reported as a fast transfer of fewer
# bytes, which reads as a slow link when it is a flaky one.
fetch() {
    local url=$1 out=$2
    curl -fsSL --retry 3 --retry-all-errors --retry-delay 1 \
        --max-time 600 -o "$out" \
        -w '%{size_download} %{time_total} %{speed_download} %{http_code}' \
        "$url" 2>/dev/null || true
}

# --- download ---------------------------------------------------------------

echo "== download (the release asset every trial fetches)" >&2
single=$(fetch "$RELEASE_URL" "$WORK/release.tar.gz")
single_bytes=$(printf '%s' "$single" | cut -d' ' -f1)
single_speed=$(printf '%s' "$single" | cut -d' ' -f3)
single_code=$(printf '%s' "$single" | cut -d' ' -f4)

if [ "${single_bytes:-0}" -le 0 ] 2>/dev/null; then
    echo "error: the release asset did not download (HTTP ${single_code:-none}).
       The rate cannot be measured from a failed transfer, and a URL that
       returns 404 also looks like a network problem." >&2
    exit 1
fi

# Four streams at once. If the total is about the same as one stream, the cap
# is per connection and more parallelism buys nothing; if it scales, it is a
# link-wide cap. Both are useful, and they are different diagnoses.
echo "== download (4 parallel streams)" >&2
par_start=$(python3 -c 'import time; print(time.time())')
for i in 1 2 3 4; do
    fetch "$RELEASE_URL" "$WORK/par$i" >"$WORK/par$i.out" &
done
wait
par_end=$(python3 -c 'import time; print(time.time())')
par_bytes=0
for i in 1 2 3 4; do
    n=$(cut -d' ' -f1 "$WORK/par$i.out" 2>/dev/null || echo 0)
    par_bytes=$((par_bytes + ${n:-0}))
done
par_speed=$(python3 -c "print(int($par_bytes / max(0.001, $par_end - $par_start)))")

# --- latency ----------------------------------------------------------------

# A single GET's `time_connect` is the cheapest honest latency probe, and it
# also catches a proxy adding a second per request — which on a task that makes
# thousands of model calls is a real cost, and on one that does not is nothing.
latency=$(curl -fsS -o /dev/null --max-time 30 \
    -w '%{time_connect} %{time_appconnect}' "$RELEASE_URL" 2>/dev/null || echo "0 0")
connect_s=$(printf '%s' "$latency" | cut -d' ' -f1)
tls_s=$(printf '%s' "$latency" | cut -d' ' -f2)

# --- container --------------------------------------------------------------

container_speed=""
container_note=""
if [ "$WITH_CONTAINER" -eq 1 ]; then
    if have docker && docker info >/dev/null 2>&1; then
        echo "== download (inside a container)" >&2
        # The same two things a trial does: pull an image, and download the
        # release. A host that is fast and a container that is slow is a proxy
        # or a Docker network setting, and only this measurement separates them.
        pull_start=$(python3 -c 'import time; print(time.time())')
        if docker pull -q alpine >/dev/null 2>&1; then
            pull_end=$(python3 -c 'import time; print(time.time())')
            container_note=$(python3 -c "print(f'image pull wall clock {$pull_end - $pull_start:.1f}s')")
        else
            container_note="image pull failed (registry unreachable from docker)"
        fi
        c=$(docker run --rm alpine sh -c \
            "apk add -q curl >/dev/null 2>&1 && \
             curl -fsSL --retry 2 --retry-all-errors --max-time 600 -o /dev/null \
             -w '%{size_download} %{speed_download}' '$RELEASE_URL'" 2>/dev/null || true)
        if [ -n "$c" ]; then
            c_bytes=$(printf '%s' "$c" | cut -d' ' -f1)
            container_speed=$(printf '%s' "$c" | cut -d' ' -f2)
            if [ "${c_bytes:-0}" -le 0 ] 2>/dev/null; then
                container_speed=""
                container_note="${container_note:+$container_note; }download inside container failed"
            fi
        else
            container_note="${container_note:+$container_note; }container probe failed"
        fi
    else
        container_note="docker not available or not usable by this user"
    fi
fi

# --- upload (opt-in) --------------------------------------------------------

upload_speed=""
if [ -n "$UPLOAD_URL" ]; then
    echo "== upload (1 MB to the given target)" >&2
    # 1 MB of zeroes. Nothing identifiable, and small enough not to matter to
    # the target, while still being long enough to outlast a TCP slow start.
    python3 -c "open('$WORK/up.bin','wb').write(b'\0' * 1048576)"
    up=$(curl -fsS --max-time 300 -o /dev/null -X POST --data-binary "@$WORK/up.bin" \
        -w '%{size_upload} %{speed_upload}' "$UPLOAD_URL" 2>/dev/null || echo "0 0")
    up_bytes=$(printf '%s' "$up" | cut -d' ' -f1)
    upload_speed=$(printf '%s' "$up" | cut -d' ' -f2)
    [ "${up_bytes:-0}" -gt 0 ] 2>/dev/null || upload_speed=""
fi

# --- report -----------------------------------------------------------------

fmt_rate() {
    python3 -c "
b = float('${1:-0}')
if b >= 1e6: print(f'{b/1e6:.2f} MB/s')
elif b >= 1e3: print(f'{b/1e3:.0f} KB/s')
else: print(f'{b:.0f} B/s')"
}

# Turn a rate into the one duration that decides something: can this host fetch
# the release inside the setup budget the runner passes?
#
# Two rates, deliberately. The adapter downloads the release with one `curl`,
# so the single-stream number is what decides whether agent setup fits its
# budget. Docker pulls image layers concurrently, so the parallel number is the
# honest one for planning image time — and on a link capped per connection
# (measured at 3.2x here) using the single-stream rate for images would
# overstate the cost of a sweep by that factor.
verdict() {
    python3 - "$single_speed" "$par_speed" "$RELEASE_BYTES" "$SETUP_BUDGET_SEC" "$IMAGE_BYTES" <<'PY'
import sys
single, par, release, budget, image = (float(a) for a in sys.argv[1:6])
def human(s):
    if s < 90: return f"{s:.0f}s"
    if s < 5400: return f"{s/60:.1f}min"
    return f"{s/3600:.1f}h"
if single <= 0:
    print("  verdict: no rate measured")
    raise SystemExit
rel = release / single
img = image / max(par, single)
print(f"  release {release/1e6:.1f} MB over one stream -> {human(rel)}"
      f"  (setup budget {human(budget)})")
print(f"  one task image ~{image/1e6:.0f} MB over {par/max(single,1):.1f} streams"
      f" -> {human(img)}")
if rel > budget:
    print("  VERDICT: the release alone exceeds the setup budget — a paid run")
    print("           fails in agent setup, not in the agent. Fix the link or")
    print("           raise --agent-setup-timeout-multiplier before spending.")
elif img > 1800:
    print("  VERDICT: runs, but each task's image costs half an hour or more,")
    print("           so a 54-task sweep is dominated by image pulls. Pre-pull")
    print("           the images and keep them, or use a faster host.")
else:
    print("  VERDICT: adequate for a paid sweep")
PY
}

echo
printf '%-34s %s\n' "single-stream download" "$(fmt_rate "$single_speed")"
printf '%-34s %s\n' "4-stream download" "$(fmt_rate "$par_speed")"
printf '%-34s %s\n' "  scaling vs single" "$(python3 -c "
s=float('${single_speed:-0}'); p=float('$par_speed')
print('n/a' if s<=0 else f'{p/s:.2f}x')")"
printf '%-34s %s\n' "connect / TLS (s)" "${connect_s} / ${tls_s}"
if [ -n "$container_speed" ]; then
    printf '%-34s %s\n' "in-container download" "$(fmt_rate "$container_speed")"
    [ -z "$container_note" ] || printf '%-34s %s\n' "  container note" "$container_note"
else
    printf '%-34s %s\n' "in-container download" "not measured (${container_note:-skipped})"
fi
if [ -n "$upload_speed" ]; then
    printf '%-34s %s\n' "upload" "$(fmt_rate "$upload_speed")"
else
    printf '%-34s %s\n' "upload" "not measured"
fi
echo
verdict

if [ -n "$JSON_OUT" ]; then
    python3 - "$JSON_OUT" <<PY
import json, sys
json.dump({
    "release_url": "$RELEASE_URL",
    "release_bytes": $single_bytes,
    "single_stream_bytes_per_sec": $single_speed,
    "four_stream_bytes_per_sec": $par_speed,
    "connect_sec": $connect_s,
    "tls_sec": $tls_s,
    "container_bytes_per_sec": ${container_speed:-None},
    "container_note": "$container_note",
    "upload_bytes_per_sec": ${upload_speed:-None},
    "setup_budget_sec": $SETUP_BUDGET_SEC,
}, open(sys.argv[1], "w"), indent=2)
PY
    echo "wrote $JSON_OUT" >&2
fi
