#!/usr/bin/env bash
# Full Kostya-style sweep for c-two (records / arrays / fastdb-hold).
# Run after activating the c-two uv env.
set -euo pipefail

cd "$(dirname "$0")/.."

OUT="benchmarks/results/kostya_ctwo.txt"
mkdir -p benchmarks/results
{
  echo "# Kostya-style coordinate benchmark — c-two IPC"
  echo "# $(date)"
  echo "# Schema: row_id u32 / x f64 / y f64 / z f64 / name STR"
  printf '%-8s  %-16s  %-7s  %-7s  %-7s  %-7s\n' N strategy p10_ms p50_ms p90_ms mean_ms
} > "$OUT"

for entry in "1000:1K:30" "10000:10K:30" "100000:100K:20" "1000000:1M:10" "3000000:3M:6"; do
  N="${entry%%:*}"
  rest="${entry#*:}"
  LBL="${rest%%:*}"
  ITERS="${rest##*:}"

  for V in pickle-records pickle-arrays fastdb-hold; do
    # Skip pickle-records at 3M — multi-second territory, dominated by Python loop.
    if [[ "$V" == "pickle-records" && "$N" -gt 1000000 ]]; then
      printf '%-8s  %-16s  %-7s  %-7s  %-7s  %-7s\n' "$LBL" "$V" SKIP SKIP SKIP SKIP >> "$OUT"
      continue
    fi
    echo "==> N=$LBL ($N) variant=$V iters=$ITERS"
    line=$(C2_RELAY_ADDRESS= uv run python benchmarks/kostya_ctwo_benchmark.py \
      --variant "$V" --n "$N" --iters "$ITERS" --warmup 3 2>&1 | tail -5)
    p10=$(echo "$line" | awk '/^  p10/{print $2}')
    p50=$(echo "$line" | awk '/^  p50/{print $2}')
    p90=$(echo "$line" | awk '/^  p90/{print $2}')
    mean=$(echo "$line" | awk '/^  mean/{print $2}')
    printf '%-8s  %-16s  %7s  %7s  %7s  %7s\n' "$LBL" "$V" "$p10" "$p50" "$p90" "$mean" >> "$OUT"
  done
done

echo
echo "=== Summary ==="
cat "$OUT"
