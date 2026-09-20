#!/usr/bin/env bash
# Two separate executables: production-like timing, then work/allocation probes.
set -euo pipefail
cd "$(dirname "$0")/.."
prefix=${1:?"usage: scripts/run-perf.sh perf-results/before [--tier quick|standard|stress] [--gpu] [--samples N] [--only SUBSTRING] [font flags]"}
shift
for arg in "$@"; do
    case "$arg" in
        --out|--list|--help|-h) echo "use cargo bench --bench pathological directly for $arg" >&2; exit 2 ;;
    esac
done
cargo bench --bench pathological -- "$@" --out "${prefix}-timing.json"
cargo bench --features perf-counters --bench pathological -- "$@" --out "${prefix}-work.json"
# Cargo.lock is intentionally ignored by this library; preserve this run's resolution.
cp Cargo.lock "${prefix}.Cargo.lock"
python3 scripts/perf-report.py "${prefix}-timing.json" \
    --before-work "${prefix}-work.json" --html "${prefix}.html"
