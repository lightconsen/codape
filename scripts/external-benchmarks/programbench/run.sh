#!/usr/bin/env bash
# Thin wrapper so the ProgramBench runner has the same entry point as the
# other external benchmarks.
set -euo pipefail
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
PYTHON=${PYTHON:-python3}
exec "$PYTHON" -I "$SCRIPT_DIR/run_programbench.py" "$@"
