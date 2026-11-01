#!/usr/bin/env bash
# Thin wrapper around run_swebench.py. See ../README.md.
#
# Usage:
#   scripts/external-benchmarks/swebench/run.sh --dry-run --limit 1
#   scripts/external-benchmarks/swebench/run.sh --instance astropy__astropy-12907
#
# The Python runner bootstraps its own pinned evaluation venv for a real run;
# --help and --dry-run work with only the standard library. Python's -I keeps
# the current directory out of sys.path, which matters when the working tree
# contains untrusted files.

set -euo pipefail

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
PYTHON=${PYTHON:-python3}

if ! command -v "$PYTHON" >/dev/null 2>&1; then
    echo "error: $PYTHON not found; install Python 3.10+ or set PYTHON=/path/to/python" >&2
    exit 2
fi

exec "$PYTHON" -I "$SCRIPT_DIR/run_swebench.py" "$@"
