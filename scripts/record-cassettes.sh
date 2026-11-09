#!/usr/bin/env bash
# Record the LLM responses used by the provider-backed E2E suites.
#
# The suites run against `llm-cassette`; in record mode it proxies to the
# endpoint in your Codape settings and writes one JSON cassette per request
# under crates/cli/tests/cassettes/. Those recordings are committed, and every
# later run (local and CI) replays them without a provider, key or network.
#
# Usage:
#   scripts/record-cassettes.sh            # add missing recordings
#   scripts/record-cassettes.sh --fresh    # discard old recordings first
#   scripts/record-cassettes.sh --check    # metadata, age, size, secrets
#   scripts/record-cassettes.sh --backfill # add metadata to older recordings
#   scripts/record-cassettes.sh --prune    # replay and delete unused cassettes
#   scripts/record-cassettes.sh --rekey    # re-key recordings to the current request
#   scripts/record-cassettes.sh --digests  # refresh recorded system-prompt digests
#
# --check exits non-zero for malformed files, unmatched-request markers or
# credential-like strings. Staleness and size are warnings unless
# CASSETTE_STRICT=1. --prune replays every suite with tracing; a suite whose
# replay fails is left untouched.
#
# The endpoint and key are read from $CODAPE_SETTINGS, defaulting to
# ~/.codape/settings.json and falling back to ~/.claude/settings.ds.json when
# the former has no provider configured. They are only exported to the test
# process; the script never prints them.

set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

case "${1:-}" in
  --check)
    shift
    exec python3 scripts/cassette_tool.py check "$@"
    ;;
  --backfill)
    shift
    exec python3 scripts/cassette_tool.py backfill "$@"
    ;;
  --prune)
    shift
    exec python3 scripts/cassette_tool.py prune "$@"
    ;;
  --digests)
    shift
    exec python3 scripts/cassette_tool.py backfill --digest "$@"
    ;;
  --rekey)
    # A recorded request is keyed by its whole body, so a change to a
    # request-wide setting (the output ceiling, a tool schema) re-keys every
    # conversation without changing any answer. This replays the suites with
    # tracing, matches each request to the recording it describes, and renames
    # it — no provider call and no re-record. Repeat until it reports no
    # missing requests: each pass only sees the turn that stopped the replay.
    shift
    for pass in 1 2 3 4 5 6 7 8 9 10; do
      rm -f crates/cli/tests/cassettes/*/_missing_*.json
      for suite in live_basic_chat live_bash_tool live_file_read \
                   e2e_code_agent e2e_equivalence code_agent_benchmark; do
        CODAPE_CASSETTE_TRACE=1 cargo test -q -p codape-cli --test "$suite" >/dev/null 2>&1 || true
      done
      CODAPE_CASSETTE_TRACE=1 cargo test -q -p codape-cli --test interactive_tui interactive_live >/dev/null 2>&1 || true
      count=$(find crates/cli/tests/cassettes -name '_missing_*.json' | wc -l | tr -d ' ')
      echo "pass $pass: $count request(s) without a recording"
      [ "$count" = "0" ] && exit 0
      python3 scripts/cassette_tool.py rekey --missing
    done
    echo "error: cassettes did not converge after 10 passes" >&2
    exit 1
    ;;
esac

# Prefer an explicit file, then Codape's own settings, then the Claude Code
# settings that commonly hold a proxy provider (ANTHROPIC_* in `env`).
if [ -n "${CODAPE_SETTINGS:-}" ]; then
  settings="$CODAPE_SETTINGS"
else
  settings="$HOME/.codape/settings.json"
  if ! grep -q "ANTHROPIC_BASE_URL\|base_url" "$settings" 2>/dev/null; then
    alt="$HOME/.claude/settings.ds.json"
    if [ -r "$alt" ] && grep -q "ANTHROPIC_BASE_URL" "$alt"; then
      settings="$alt"
    fi
  fi
fi
if [ ! -r "$settings" ]; then
  echo "error: cannot read $settings" >&2
  echo "point CODAPE_SETTINGS at the settings file that holds your provider" >&2
  exit 1
fi

endpoint=$(python3 - "$settings" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
env = data.get("env") or {}
provider = data.get("provider") or {}
base = provider.get("base_url") or env.get("ANTHROPIC_BASE_URL")
key = (
    data.get("api_key")
    or env.get("ANTHROPIC_API_KEY")
    or env.get("ANTHROPIC_AUTH_TOKEN")
)
if not base or not key:
    raise SystemExit(
        "settings have no Anthropic-compatible base_url/api_key (or env.ANTHROPIC_BASE_URL/AUTH_TOKEN)"
    )
print(base)
print(key)
PY
)
base_url=$(printf '%s\n' "$endpoint" | sed -n '1p')
api_key=$(printf '%s\n' "$endpoint" | sed -n '2p')
if [ -z "$base_url" ] || [ -z "$api_key" ]; then
  echo "error: could not read the endpoint from $settings" >&2
  exit 1
fi

export CODAPE_CASSETTE_MODE=record
export CODAPE_CASSETTE_UPSTREAM="$base_url"
export CODAPE_CASSETTE_API_KEY="$api_key"

if [ "${1:-}" = "--fresh" ]; then
  rm -rf crates/cli/tests/cassettes
fi
mkdir -p crates/cli/tests/cassettes

run() {
  echo "== recording $1"
  shift
  cargo test -p codape-cli "$@" -- --test-threads=1
}

run live_basic_chat --test live_basic_chat
run live_bash_tool --test live_bash_tool
run live_file_read --test live_file_read
run e2e_code_agent --test e2e_code_agent
run e2e_equivalence --test e2e_equivalence
run code_agent_benchmark --test code_agent_benchmark
run interactive_tui_live --test interactive_tui interactive_live

echo
echo "recorded cassettes:"
find crates/cli/tests/cassettes -name '*.json' | wc -l | tr -d ' '
