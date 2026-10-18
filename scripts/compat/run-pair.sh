#!/bin/bash
# Start one host and one headless viewer (pretend screens, no real capture),
# pair them, stream ~10 s, and check the three things that must keep
# working between versions:
#   1. they connect,
#   2. pictures flow (the viewer reports at least 20 fps),
#   3. input reaches the host (only when the viewer can send test input,
#      which needs 0.5.9 or newer: pass "input" as the 4th argument).
# Usage: run-pair.sh <aa-host> <aa-viewer> <label> [input]
set -u
host=$1 viewer=$2 label=$3 want_input=${4:-}
d=$(mktemp -d)
AA_DATA_DIR=$d/host AA_NO_TELEMETRY=1 "$host" --mock >"$d/host.log" 2>&1 &
h=$!
code=""
for _ in $(seq 1 40); do
  code=$(sed 's/\x1b\[[0-9;]*m//g' "$d/host.log" | grep -oE "new computers: [0-9 ]+" | tr -dc 0-9 | head -c6)
  [ ${#code} -eq 6 ] && break
  sleep 0.5
done
AA_DATA_DIR=$d/viewer AA_NO_TELEMETRY=1 AA_TEST_INPUT=1 timeout 14 "$viewer" --headless --mock --pair "$code" 127.0.0.1:7700 >"$d/viewer.log" 2>&1
kill $h 2>/dev/null; wait $h 2>/dev/null
clean() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }
fail=""
clean "$d/viewer.log" | grep -q " connected" || fail="never connected"
fps=$(clean "$d/viewer.log" | grep -oE "stream fps=[0-9]+" | tail -3 | head -1 | tr -dc 0-9)
[ -z "$fail" ] && [ "${fps:-0}" -lt 20 ] && fail="pictures not flowing (fps=${fps:-none})"
if [ -z "$fail" ] && [ "$want_input" = input ]; then
  clean "$d/host.log" | grep -q "mock input" || fail="input never reached the host"
fi
if [ -n "$fail" ]; then
  echo "FAIL $label: $fail"
  echo "--- host log (last 20 lines)"; clean "$d/host.log" | tail -20
  echo "--- viewer log (last 20 lines)"; clean "$d/viewer.log" | tail -20
  exit 1
fi
echo "ok   $label: connected, ${fps} fps${want_input:+, input reached the host}"
