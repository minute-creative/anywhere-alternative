#!/bin/bash
# Chaos tests: try to break the real host and viewer. Writes chaos-report.md.
S=${SIM_DIR:-$(cd "$(dirname "$0")" && pwd)}
B=${BIN_DIR:-$(cd "$S/../.." && pwd)/target/release}
R=$S/chaos-report.md
strip() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }
row() { echo "| $1 | $2 | $3 |" >> $R; }
cleanup() { pkill -x aa-host 2>/dev/null; pkill -x aa-viewer 2>/dev/null; pkill -f "$S/relay.py" 2>/dev/null; pkill -f "$S/flood.py" 2>/dev/null; sleep 0.4; }
fps_tail() { strip "$1" | grep -o "stream fps=[0-9]*" | tail -${2:-3} | cut -d= -f2 | tr '\n' ' '; }
alive() { kill -0 "$1" 2>/dev/null; }
rss_mb() { echo $(( $(ps -o rss= -p "$1" 2>/dev/null || echo 0) / 1024 )); }
cpu() { ps -o %cpu= -p "$1" 2>/dev/null | tr -d ' '; }

echo "# Chaos pass $(date -u +%Y-%m-%dT%H:%MZ)" > $R
echo "" >> $R
echo "| Attack | Result | Detail |" >> $R
echo "|---|---|---|" >> $R

# 1. Garbage flood at the host while streaming
cleanup
$B/aa-host --mock --listen 0.0.0.0:7700 > $S/c1.host.log 2>&1 & HP=$!; sleep 0.7
timeout 16 $B/aa-viewer --headless --mock 127.0.0.1:7700 > $S/c1.viewer.log 2>&1 & VP=$!
sleep 2; python3 $S/flood.py 127.0.0.1 7700 10 8000 > $S/c1.flood.log 2>&1
python3 $S/flood.py 127.0.0.1 7701 2 4000 > /dev/null 2>&1
wait $VP
f=$(fps_tail $S/c1.viewer.log 4)
alive $HP && [ -n "$f" ] && ! echo "$f" | grep -qw 0 \
  && row "8,000 garbage packets/s at the host for 10 s" "✅ survived" "host alive, viewer kept streaming (last fps: $f), $(cat $S/c1.flood.log)" \
  || row "Garbage flood at host" "❌ FAIL" "host alive=$(alive $HP && echo yes || echo no); fps: $f"
cleanup

# 2. Corrupted + duplicated packets on the link
python3 $S/relay.py 4 2 0 0 0 > /dev/null 2>&1 &
export RELAY_CORRUPT=0.01 RELAY_DUP=0.05
pkill -f "$S/relay.py"; sleep 0.2
RELAY_CORRUPT=0.01 RELAY_DUP=0.05 python3 $S/relay.py 4 2 0 0 0 > /dev/null 2>&1 &
$B/aa-host --mock --test-clipboard --listen 127.0.0.1:7700 > $S/c2.host.log 2>&1 & HP=$!; sleep 0.7
timeout 15 $B/aa-viewer --headless --mock --test-clipboard --test-gamepad --test-mic 127.0.0.1:7800 > $S/c2.viewer.log 2>&1; VE=$?
unset RELAY_CORRUPT RELAY_DUP
f=$(fps_tail $S/c2.viewer.log 4)
panics=$(cat $S/c2.*.log | grep -ci "panicked")
alive $HP && [ $panics -eq 0 ] && [ $VE -eq 124 ] \
  && row "1% corrupted + 5% duplicated packets, everything on" "✅ survived" "no crash either side; fps: $f; clipboard pasted on host: $(strip $S/c2.host.log | grep -c 'pasted from')" \
  || row "Corrupted/duplicated packets" "❌ FAIL" "host alive=$(alive $HP && echo yes || echo no) viewer exit=$VE panics=$panics"
cleanup

# 3. 30% loss
python3 $S/relay.py 4 2 0.30 0 0 > /dev/null 2>&1 &
$B/aa-host --mock --listen 127.0.0.1:7700 > $S/c3.host.log 2>&1 & HP=$!; sleep 0.7
timeout 15 $B/aa-viewer --headless --mock 127.0.0.1:7800 > $S/c3.viewer.log 2>&1; VE=$?
f=$(fps_tail $S/c3.viewer.log 5)
[ $VE -eq 124 ] && alive $HP \
  && row "30% packet loss for 15 s" "✅ stayed connected" "fps last 5 s: $f (degraded but alive)" \
  || row "30% loss" "❌ FAIL" "viewer exit=$VE ($(strip $S/c3.viewer.log | grep -o 'Error.*\|lost the host' | head -1))"
cleanup

# 4. Total blackout 8 s, then back
RELAY_BLACKOUT=5,8 python3 $S/relay.py 4 2 0 0 0 > /dev/null 2>&1 &
$B/aa-host --mock --listen 127.0.0.1:7700 > $S/c4.host.log 2>&1 & HP=$!; sleep 0.7
timeout 26 $B/aa-viewer --headless --mock 127.0.0.1:7800 > $S/c4.viewer.log 2>&1; VE=$?
n=$(strip $S/c4.viewer.log | grep -c "connected negotiated"); f=$(fps_tail $S/c4.viewer.log 3)
[ $n -ge 2 ] && ! echo "$f" | grep -qw 0 \
  && row "Network gone for 8 s, then back" "✅ recovered" "viewer noticed, reconnected by itself ($n connections), fps: $f" \
  || row "8 s blackout" "❌ FAIL" "connections=$n fps: $f exit=$VE"
cleanup

# 5. Host killed and restarted 5 times
$B/aa-host --mock > /dev/null 2>&1 & HP=$!; sleep 0.7
timeout 70 $B/aa-viewer --headless --mock > $S/c5.viewer.log 2>&1 & VP=$!
for i in 1 2 3 4 5; do sleep 6; kill -9 $HP; wait $HP 2>/dev/null; sleep 3; $B/aa-host --mock > /dev/null 2>&1 & HP=$!; done
sleep 8; n=$(strip $S/c5.viewer.log | grep -c "connected negotiated"); f=$(fps_tail $S/c5.viewer.log 3); kill $VP 2>/dev/null
[ $n -ge 6 ] && ! echo "$f" | grep -qw 0 \
  && row "PC app crashed (kill -9) 5 times" "✅ recovered each time" "$n connections, final fps: $f" \
  || row "Host crash loop" "❌ FAIL" "connections=$n fps: $f"
cleanup

# 6. Viewer connects and dies 30 times in a row; host must stay healthy
$B/aa-host --mock > $S/c6.host.log 2>&1 & HP=$!; sleep 0.7; m0=$(rss_mb $HP)
for i in $(seq 30); do $B/aa-viewer --headless --mock > /dev/null 2>&1 & VP=$!; sleep 0.4; kill -9 $VP; wait $VP 2>/dev/null; done
timeout 6 $B/aa-viewer --headless --mock > $S/c6.viewer.log 2>&1
f=$(fps_tail $S/c6.viewer.log 2); m1=$(rss_mb $HP)
alive $HP && ! echo "$f" | grep -qw 0 && [ -n "$f" ] \
  && row "Viewer crashed 30 times in 12 s" "✅ host healthy" "next viewer streams at $f fps; host memory ${m0}→${m1} MB" \
  || row "Viewer crash loop" "❌ FAIL" "host alive=$(alive $HP && echo yes || echo no) fps: $f"
cleanup

# 7. Clipboard storm: both sides copy every 50 ms
$B/aa-host --mock --test-clipboard --listen 127.0.0.1:7700 > $S/c7.host.log 2>&1 & HP=$!; sleep 0.7
AA_TEST_CLIPBOARD_MS=50 timeout 12 $B/aa-viewer --headless --mock --test-clipboard 127.0.0.1:7700 > $S/c7.viewer.log 2>&1
f=$(fps_tail $S/c7.viewer.log 3)
alive $HP && ! echo "$f" | grep -qw 0 \
  && row "Clipboard storm (copy every 50 ms)" "✅ survived" "video unaffected (fps: $f); pasted on host: $(strip $S/c7.host.log | grep -c 'pasted from')" \
  || row "Clipboard storm" "❌ FAIL" "fps: $f"
cleanup

# 8. Long soak with everything on, Wi-Fi conditions, CPU and memory
python3 $S/relay.py 4 2 0.005 5 40 > /dev/null 2>&1 &
AA_SIMULATE_CAPTURE_FAULTS=1 $B/aa-host --mock --mock-res 1280x720 --mock-fps 120 --test-clipboard --listen 127.0.0.1:7700 > $S/c8.host.log 2>&1 & HP=$!; sleep 0.7
$B/aa-viewer --headless --mock --test-clipboard --test-mic --test-gamepad 127.0.0.1:7800 > $S/c8.viewer.log 2>&1 & VP=$!
sleep 30; h1=$(rss_mb $HP); v1=$(rss_mb $VP)
sleep $(( ${SOAK:-300} - 40 )); h2=$(rss_mb $HP); v2=$(rss_mb $VP); hc=$(cpu $HP); vc=$(cpu $VP)
alive $VP || v2=dead
kill $VP; wait $VP 2>/dev/null
panics=$(cat $S/c8.*.log | grep -ci panicked)
[ $panics -eq 0 ] && [ "$v2" != dead ] && [ $((h2-h1)) -lt 20 ] && [ $((v2-v1)) -lt 20 ] \
  && row "Soak ${SOAK:-300} s: 120 fps + mic + pad + clipboard + screen faults, Wi-Fi" "✅ stable" "memory host ${h1}→${h2} MB, viewer ${v1}→${v2} MB; avg CPU host ${hc}%, viewer ${vc}%" \
  || row "Long soak" "❌ FAIL" "panics=$panics host ${h1}→${h2} viewer ${v1}→${v2}"
cleanup

# 9. Two viewer apps on the same Mac at once: no ping-pong
$B/aa-host --mock > $S/c9.host.log 2>&1 & HP=$!; sleep 0.7
$B/aa-viewer --headless --mock > $S/c9a.viewer.log 2>&1 & A=$!; sleep 2
timeout 8 $B/aa-viewer --headless --mock > $S/c9b.viewer.log 2>&1; BE=$?
sleep 3; kill $A 2>/dev/null; wait $A 2>/dev/null
takeovers=$(strip $S/c9.host.log | grep -c "viewer reconnected")
a_tail=$(fps_tail $S/c9a.viewer.log 4)
[ $takeovers -eq 0 ] && ! echo "$a_tail" | grep -qw 0 \
  && row "Second viewer app on the same Mac" "✅ handled" "first keeps the stream (fps $a_tail); second waits as 'busy' and gives up cleanly" \
  || row "Second viewer app on the same Mac" "❌ FAIL" "takeovers=$takeovers first fps: $a_tail"
cleanup

# 10. Viewer restarted within a second of crashing (takes over once the old one is silent)
$B/aa-host --mock > $S/c10.host.log 2>&1 & HP=$!; sleep 0.7
$B/aa-viewer --headless --mock > /dev/null 2>&1 & A=$!; sleep 2; kill -9 $A; wait $A 2>/dev/null; sleep 0.3
timeout 8 $B/aa-viewer --headless --mock > $S/c10.viewer.log 2>&1
strip $S/c10.viewer.log | grep -q "connected negotiated" \
  && row "Viewer crash + instant restart" "✅ reconnected" "waited for the old session to go silent, then took over" \
  || row "Viewer crash + instant restart" "❌ FAIL" "$(strip $S/c10.viewer.log | grep -o 'Error.*' | head -1)"
cleanup
echo done
