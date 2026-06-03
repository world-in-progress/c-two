#!/usr/bin/env bash
# Full Kostya-style sweep for c-two (pickle fallback / FastDB control / FastDB recommended / retained response).
# Run after activating the c-two uv env.
set -euo pipefail

cd "$(dirname "$0")/../../.."

label_for_variant() {
  case "$1" in
    pickle-records) echo "pickle records" ;;
    pickle-arrays) echo "pickle arrays" ;;
    fastdb-control-default) echo "Batch.allocate control default" ;;
    fastdb-control-retained) echo "Batch.allocate control retained" ;;
    fastdb-control-retained-unsafe) echo "Batch.allocate control retained unsafe" ;;
    fastdb-require-default) echo "fdb.require recommended default" ;;
    fastdb-require-retained) echo "fdb.require recommended retained" ;;
    fastdb-require-retained-unsafe) echo "fdb.require recommended retained unsafe" ;;
    fastdb-numeric-control-default) echo "numeric control default" ;;
    fastdb-numeric-control-retained) echo "numeric control retained" ;;
    fastdb-numeric-control-retained-unsafe) echo "numeric control retained unsafe" ;;
    fastdb-numeric-require-runtime-default) echo "numeric fdb.require runtime default" ;;
    fastdb-numeric-require-runtime-retained) echo "numeric fdb.require runtime retained" ;;
    fastdb-numeric-require-runtime-retained-unsafe) echo "numeric fdb.require runtime retained unsafe" ;;
    *) echo "$1" ;;
  esac
}

OUT="sdk/python/benchmarks/results/kostya_ctwo.txt"
mkdir -p sdk/python/benchmarks/results
{
  echo "# Kostya-style coordinate benchmark — c-two IPC"
  echo "# $(date)"
  echo "# Schema: row_id u32 / x f64 / y f64 / z f64 / name STR"
  echo "# control uses ordinary Batch.allocate(...); recommended uses fdb.require(...)."
  echo "# CLI variant selectors are kept in kostya_ctwo_benchmark.py; this file reports reader-facing labels."
  printf '%-8s  %-48s  %-7s  %-7s  %-7s  %-7s\n' N strategy p10_ms p50_ms p90_ms mean_ms
} > "$OUT"

for entry in "1000:1K:30" "10000:10K:30" "100000:100K:20" "1000000:1M:10" "3000000:3M:6"; do
  N="${entry%%:*}"
  rest="${entry#*:}"
  LBL="${rest%%:*}"
  ITERS="${rest##*:}"

  for V in \
    pickle-records pickle-arrays \
    fastdb-control-default fastdb-control-retained fastdb-control-retained-unsafe \
    fastdb-require-default fastdb-require-retained fastdb-require-retained-unsafe \
    fastdb-numeric-control-default fastdb-numeric-control-retained fastdb-numeric-control-retained-unsafe \
    fastdb-numeric-require-runtime-default fastdb-numeric-require-runtime-retained fastdb-numeric-require-runtime-retained-unsafe; do
    # Skip pickle-records at 3M — multi-second territory, dominated by Python loop.
    LABEL="$(label_for_variant "$V")"
    if [[ "$V" == "pickle-records" && "$N" -gt 1000000 ]]; then
      printf '%-8s  %-48s  %-7s  %-7s  %-7s  %-7s\n' "$LBL" "$LABEL" SKIP SKIP SKIP SKIP >> "$OUT"
      continue
    fi
    echo "==> N=$LBL ($N) variant=$V iters=$ITERS"
    line=$(C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/kostya_ctwo_benchmark.py \
      --variant "$V" --n "$N" --iters "$ITERS" --warmup 3 2>&1 | tail -5)
    p10=$(echo "$line" | awk '/^  p10/{print $2}')
    p50=$(echo "$line" | awk '/^  p50/{print $2}')
    p90=$(echo "$line" | awk '/^  p90/{print $2}')
    mean=$(echo "$line" | awk '/^  mean/{print $2}')
    printf '%-8s  %-48s  %7s  %7s  %7s  %7s\n' "$LBL" "$LABEL" "$p10" "$p50" "$p90" "$mean" >> "$OUT"
  done
done

echo
echo "=== Summary ==="
cat "$OUT"
