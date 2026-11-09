#!/usr/bin/env bash
# Generate workspace line coverage and a per-crate summary.
#
# Usage: scripts/coverage.sh [--summary-only] [output-dir]
#
# Writes lcov.info, coverage.tsv and summary.md under the output directory
# (default target/coverage). Test files under crates/*/tests, benches and
# examples are excluded from the denominator. The thresholds are advisory: this
# script always exits 0 after a successful test run so coverage never blocks
# local work; CI can turn the summary into a required check later.
#
# --summary-only regenerates the tables from an existing lcov.info.

set -euo pipefail

cd "$(dirname "$0")/.."
summary_only=false
if [[ "${1:-}" == "--summary-only" ]]; then
    summary_only=true
    shift
fi
out="${1:-target/coverage}"
mkdir -p "$out"

if [[ "$summary_only" == false ]]; then
    # Two calls, for the same reason the `Test` job uses two: one `nextest` call
    # over the whole workspace starts all 125 test binaries at once, and on the
    # hosted runner that ends the job — `nextest` reports "Cancelling due to
    # signal" 1.2-1.6s after "Starting 4685 tests across 125 binaries", always
    # at index 424, and the runner's own cancellation is what arrived first.
    # Lowering `--test-threads` moves the instant but not the index, so this is
    # not a concurrency limit; splitting the call is what completes. Both halves
    # run the whole suite exactly once (3967 + 718 = 4685).
    #
    # The split does not remove that class of failure, it only moves where it
    # lands: the second call can be cancelled the same way (runner shutdown
    # signal, 0.05-0.07s after "Starting 718 tests", index 13-16). That is a
    # hosted-runner problem on `main` too, tracked in issue #25, not a result of
    # these tests — do not read a cancelled run here as a coverage regression.
    #
    # `llvm-cov` merges every profile under the target directory when the report
    # is generated, so each call runs with `--no-report` and the report is
    # produced once at the end. Generating it per call would export only that
    # call's profiles and the second would overwrite the first.
    if cargo nextest --version >/dev/null 2>&1; then
        cargo llvm-cov nextest --workspace --no-fail-fast --locked --no-report \
            -E 'not package(codape-cli)'
        cargo llvm-cov nextest -p codape-cli --no-fail-fast --locked --no-report
        cargo llvm-cov report --lcov --output-path "$out/lcov.info"
    else
        echo "cargo-nextest is not installed; falling back to 'cargo test'," >&2
        echo "which shares one process group between every test binary." >&2
        echo "Install it (cargo install cargo-nextest --locked) for isolation." >&2
        cargo llvm-cov --workspace --no-fail-fast --locked --lcov --output-path "$out/lcov.info"
    fi
fi
if [[ ! -f "$out/lcov.info" ]]; then
    echo "missing $out/lcov.info; run without --summary-only first" >&2
    exit 1
fi

awk -F: '
    function crate_of(path, rest, parts) {
        if (path !~ /\/crates\/[^\/]+\//) return "other"
        rest = path
        sub(/^.*\/crates\//, "", rest)
        split(rest, parts, "/")
        return parts[1]
    }
    /^SF:/ {
        file = substr($0, 4)
        next
    }
    /^LF:/ {
        lf = $2
        next
    }
    /^LH:/ {
        lh = $2
        next
    }
    /^end_of_record/ {
        if (file != "" &&
            file !~ /\/tests\// &&
            file !~ /\/benches\// &&
            file !~ /\/examples\//) {
            crate = crate_of(file)
            total[crate] += lf
            hit[crate] += lh
        }
        file = ""
        next
    }
    END {
        for (crate in total) {
            printf "%s\t%d\t%d\n", crate, hit[crate], total[crate]
        }
    }
' "$out/lcov.info" | sort > "$out/coverage.tsv"

{
    printf '| crate | lines hit | lines | coverage |\n'
    printf '|---|---:|---:|---:|\n'
    awk -F '\t' '{
        if ($3 > 0) printf "| %s | %d | %d | %.1f%% |\n", $1, $2, $3, 100 * $2 / $3
    }' "$out/coverage.tsv"
} | tee "$out/summary.md"

echo
awk -F '\t' '{
    if ($3 == 0) next
    pct = 100 * $2 / $3
    target = ($1 == "core" || $1 == "cli" || $1 == "tools") ? 80 : 65
    if (pct < target) printf "below target: %-12s %.1f%% (target %d%%)\n", $1, pct, target
}' "$out/coverage.tsv"

echo
echo "lcov:    $out/lcov.info"
echo "tsv:     $out/coverage.tsv"
echo "summary: $out/summary.md"
echo "targets: core/cli/tools >= 80% lines; other crates >= 65% lines (advisory)"
