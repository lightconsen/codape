#!/usr/bin/env bash
# Thin wrapper around run_terminal_bench.py. See ../README.md.
#
# Usage:
#   scripts/external-benchmarks/terminal-bench/run.sh --dry-run --limit 1
#   scripts/external-benchmarks/terminal-bench/run.sh --task hello-world
#
# The Python runner bootstraps a pinned terminal-bench venv (Python 3.12+) for
# a real run; --help and --dry-run work with only the standard library.
# PYTHON may point at a 3.12+ interpreter when uv is not installed.

set -euo pipefail

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
PYTHON=${PYTHON:-python3}

if ! command -v "$PYTHON" >/dev/null 2>&1; then
    echo "error: $PYTHON not found; install Python 3.12+ or set PYTHON=/path/to/python" >&2
    exit 2
fi

exec "$PYTHON" -I "$SCRIPT_DIR/run_terminal_bench.py" "$@"
