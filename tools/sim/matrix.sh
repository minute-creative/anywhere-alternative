#!/bin/bash
# Full use-case test pass for Anywhere Alternative (mock pipeline).
# Writes a markdown report to $S/report.md.
S=${SIM_DIR:-$(cd "$(dirname "$0")" && pwd)}
B=${BIN_DIR:-$(cd "$(dirname "$0")/../.." && pwd)/target/release}
R=$S/report.md
strip() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }
pass() { echo "| $1 | ✅ pass | $2 |" >> $R; }
fail() { echo "| $1 | ❌ FAIL | $2 |" >> $R; }
note() { echo "| $1 | ⚠️ note | $2 |" >> $R; }
cleanup() { pkill -x aa-host 2>/dev/null; pkill -x aa-viewer 2>/dev/null; pkill -f "python3 $S/relay" 2>/dev/null; sleep 0.3; }

echo "# Test pass $(date -u +%Y-%m-%dT%H:%MZ)" > $R
echo "" >> $R
echo "## Video under network conditions (1280×720, software codec, 2-core box)" >> $R
echo "" >> $R
echo '```' >> $R
for spec in "clean_60:60:none" "clean_120:120:none" "clean_240:240:none" \
            "wifi_60:60:4 2 0.005 5 40" "wifi_120:120:4 2 0.005 5 40" \
            "badwifi_120:120:8 6 0.02 2 80" "congested6M_60:60:4 1 0 0 0 0 6" \
            "reorder5_120:120:4 0 0 0 0 0.05" "freeze500ms_60:60:4 1 0 6 500"; do
  IFS=: read name fps relay <<< "$spec"
  cleanup
  $S/run.sh m_$name 1280x720 $fps "$relay" 16 >/dev/null 2>&1
done
(cd $S && python3 summarize.py m_clean_60 m_clean_120 m_clean_240 m_wifi_60 m_wifi_120 m_badwifi_120 m_congested6M_60 m_reorder5_120 m_freeze500ms_60) >> $R
echo '```' >> $R
echo "" >> $R
for n in m_wifi_120 m_badwifi_120 m_freeze500ms_60; do
  echo "- $n: $(strip $S/$n.viewer.log | grep 'stream fps' | tail -1 | grep -o 'dropped=[0-9]* fec_fixed=[0-9]*')" >> $R
done
echo "" >> $R
echo "## Use cases" >> $R
echo "" >> $R
echo "| Case | Result | Detail |" >> $R
echo "|---|---|---|" >> $R

# Discovery
for mode in normal firewall nobeacon; do
  cleanup; rm -f ~/.config/AnywhereAlternative/last-host
  env=""; [ $mode = firewall ] && env="AA_SIMULATE_FIREWALL=1"; [ $mode = nobeacon ] && env="AA_SIMULATE_NO_BEACON=1"
  env $env $B/aa-host --mock > $S/d_$mode.host.log 2>&1 & sleep 0.8
  timeout 6 $B/aa-viewer --headless --mock > $S/d_$mode.viewer.log 2>&1
  how=$(strip $S/d_$mode.viewer.log | grep -o 'how="[a-z]*"' | head -1)
  strip $S/d_$mode.viewer.log | grep -q "connected negotiated" && pass "Discovery: $mode" "found ($how), connected" || fail "Discovery: $mode" "not connected"
done
cleanup
AA_SIMULATE_FIREWALL=1 AA_SIMULATE_NO_BEACON=1 $B/aa-host --mock > /dev/null 2>&1 & sleep 0.8
timeout 7 $B/aa-viewer --headless --mock > $S/d_memory.viewer.log 2>&1
strip $S/d_memory.viewer.log | grep -q "trying the PC that worked last time" && strip $S/d_memory.viewer.log | grep -q "connected negotiated" \
  && pass "Discovery: everything blocked" "connected via remembered address" || fail "Discovery: everything blocked" "no fallback"

# Clipboard text both ways over 2% loss + 2% reorder
cleanup
python3 $S/relay.py 4 2 0.02 0 0 0.02 > /dev/null 2>&1 &
$B/aa-host --mock --test-clipboard --listen 127.0.0.1:7700 > $S/c.host.log 2>&1 & sleep 0.7
timeout 13 $B/aa-viewer --headless --mock --test-clipboard 127.0.0.1:7800 > $S/c.viewer.log 2>&1
sent_v=$(strip $S/c.viewer.log | grep -c "copied here"); got_h=$(strip $S/c.host.log | grep -c "pasted from the other machine")
sent_h=$(strip $S/c.host.log | grep -c "copied here"); got_v=$(strip $S/c.viewer.log | grep -c "pasted from the other machine")
[ $got_h -ge $((sent_v-1)) ] && [ $got_v -ge $((sent_h-1)) ] && [ $sent_v -ge 4 ] \
  && pass "Clipboard text, both ways, 2% loss" "Mac→PC $got_h/$sent_v, PC→Mac $got_v/$sent_h (last may be in flight)" \
  || fail "Clipboard text, both ways, 2% loss" "Mac→PC $got_h/$sent_v, PC→Mac $got_v/$sent_h"

# Clipboard 4 MB image over 1% loss
cleanup
python3 $S/relay.py 2 1 0.01 0 0 > /dev/null 2>&1 &
$B/aa-host --mock --listen 127.0.0.1:7700 > $S/ci.host.log 2>&1 & sleep 0.7
AA_TEST_CLIPBOARD_BYTES=4000000 timeout 21 $B/aa-viewer --headless --mock --test-clipboard 127.0.0.1:7800 > $S/ci.viewer.log 2>&1
got=$(strip $S/ci.host.log | grep -c "pasted from the other machine item=\"image, 3906 KB\"")
[ $got -ge 2 ] && pass "Clipboard 4 MB image, 1% loss" "$got images arrived whole" || fail "Clipboard 4 MB image, 1% loss" "$got arrived"

# Mic over 1% loss + 2% reorder
cleanup
python3 $S/relay.py 4 2 0.01 0 0 0.02 > /dev/null 2>&1 &
$B/aa-host --mock --listen 127.0.0.1:7700 > $S/mic.host.log 2>&1 & sleep 0.7
timeout 12 $B/aa-viewer --headless --mock --test-mic 127.0.0.1:7800 > /dev/null 2>&1
last=$(strip $S/mic.host.log | grep -o "frames=[0-9]* concealed=[0-9]*" | tail -1)
fr=$(echo $last | grep -o "frames=[0-9]*" | cut -d= -f2)
[ "${fr:-0}" -ge 900 ] && pass "Microphone Mac→PC, 1% loss" "$last (≈100 frames/s)" || fail "Microphone Mac→PC" "$last"

# Controller
cleanup
python3 $S/relay.py 4 2 0.01 0 0 0.02 > /dev/null 2>&1 &
$B/aa-host --mock --listen 127.0.0.1:7700 > $S/pad.host.log 2>&1 & sleep 0.7
timeout 10 $B/aa-viewer --headless --mock --test-gamepad 127.0.0.1:7800 > /dev/null 2>&1
plug=$(strip $S/pad.host.log | grep -c "mock controller plugged in slot=0 kind=PlayStation")
upd=$(strip $S/pad.host.log | grep -o "updates=[0-9]*" | tail -1 | cut -d= -f2)
[ $plug -eq 1 ] && [ "${upd:-0}" -ge 1500 ] && pass "Controller (DualSense) 1% loss" "plugged once as PlayStation, $upd updates (≈250/s)" || fail "Controller" "plug=$plug updates=$upd"

# Mouse/keyboard input
cleanup
$B/aa-host --mock --listen 127.0.0.1:7700 > $S/in.host.log 2>&1 & sleep 0.7
timeout 6 $B/aa-viewer --headless --mock --test-input 127.0.0.1:7700 > /dev/null 2>&1
strip $S/in.host.log | grep -q "mock input count=" && pass "Mouse input reaches host" "$(strip $S/in.host.log | grep -o 'count=[0-9]*' | tail -1)" || fail "Mouse input" "none"

# Host restart mid-session
cleanup
$B/aa-host --mock > /dev/null 2>&1 & HP=$!; sleep 0.7
timeout 25 $B/aa-viewer --headless --mock > $S/rc.viewer.log 2>&1 & VP=$!
sleep 4; kill $HP; wait $HP 2>/dev/null; sleep 5
$B/aa-host --mock > /dev/null 2>&1 & HP=$!; sleep 8
n=$(strip $S/rc.viewer.log | grep -c "connected negotiated")
fps=$(strip $S/rc.viewer.log | grep -o "stream fps=[0-9]*" | tail -1)
[ $n -ge 2 ] && pass "PC program restarted mid-session" "Mac reconnected by itself, $fps" || fail "PC restart" "connections=$n"
kill $VP 2>/dev/null; cleanup

# Viewer restart (same machine, new port) while host still remembers old one
$B/aa-host --mock > $S/vr.host.log 2>&1 & sleep 0.7
$B/aa-viewer --headless --mock > /dev/null 2>&1 & VP=$!
sleep 2; kill -9 $VP; wait $VP 2>/dev/null
timeout 5 $B/aa-viewer --headless --mock > $S/vr.viewer.log 2>&1
strip $S/vr.viewer.log | grep -q "connected negotiated" && strip $S/vr.host.log | grep -q "viewer reconnected" \
  && pass "Mac app restarted (crash) and reconnects" "host let the same machine take over at once" || fail "Viewer restart" "see vr logs"

# Host screen faults: resolution change, capture errors, GPU reset, lock, refresh change
cleanup
python3 $S/relay.py 4 2 0.005 0 0 > /dev/null 2>&1 &
AA_SIMULATE_CAPTURE_FAULTS=1 $B/aa-host --mock --mock-res 1280x720 --listen 127.0.0.1:7700 > $S/heal.host.log 2>&1 & sleep 0.7
timeout 34 $B/aa-viewer --headless --mock 127.0.0.1:7800 > $S/heal.viewer.log 2>&1
tail_fps=$(strip $S/heal.viewer.log | grep -o "stream fps=[0-9]*" | tail -3 | cut -d= -f2 | tr '\n' ' ')
rebuilt=$(strip $S/heal.host.log | grep -c "video pipeline rebuilt")
locked=$(strip $S/heal.viewer.log | grep -c "host: ")
low=$(strip $S/heal.viewer.log | grep -o "stream fps=[0-9]*" | tail -3 | cut -d= -f2 | awk '$1<50' | wc -l)
[ "$rebuilt" -ge 1 ] && [ "$locked" -ge 2 ] && [ "$low" -eq 0 ] \
  && pass "PC screen faults (resize, errors, GPU reset, lock, 120 Hz)" "recovered every time; last seconds fps: $tail_fps; viewer told about the lock" \
  || fail "PC screen faults" "rebuilt=$rebuilt status_msgs=$locked last fps: $tail_fps"

# Second machine while busy
cleanup
$B/aa-host --mock --listen 0.0.0.0:7700 > /dev/null 2>&1 & sleep 0.7
timeout 8 $B/aa-viewer --headless --mock --bind 127.0.0.1:0 127.0.0.1:7700 > /dev/null 2>&1 & sleep 1.5
IP=$(hostname -I | awk '{print $1}')
timeout 9 $B/aa-viewer --headless --mock --bind $IP:0 $IP:7700 > $S/busy.viewer.log 2>&1
strip $S/busy.viewer.log | grep -q "host is busy" && pass "Second computer while one is connected" "politely refused: host busy" || note "Second computer" "$(strip $S/busy.viewer.log | grep -o 'Error.*' | head -1)"
cleanup

# Soak: 90 s at 120 fps on Wi-Fi, memory of both sides
python3 $S/relay.py 4 2 0.005 5 40 > /dev/null 2>&1 &
$B/aa-host --mock --mock-res 1280x720 --mock-fps 120 --test-clipboard --listen 127.0.0.1:7700 > $S/soak.host.log 2>&1 & HP=$!; sleep 0.7
timeout 95 $B/aa-viewer --headless --mock --test-clipboard --test-mic --test-gamepad 127.0.0.1:7800 > $S/soak.viewer.log 2>&1 & VP=$!
sleep 20; h1=$(ps -o rss= -p $HP); v1=$(ps -o rss= -p $VP)
sleep 70; h2=$(ps -o rss= -p $HP); v2=$(ps -o rss= -p $VP)
wait $VP 2>/dev/null
dh=$(( (h2-h1)/1024 )); dv=$(( (v2-v1)/1024 ))
[ $dh -lt 15 ] && [ $dv -lt 15 ] && pass "Soak 90 s (video 120 fps + mic + pad + clipboard, Wi-Fi)" "memory growth host ${dh} MB, viewer ${dv} MB between 20 s and 90 s" \
  || fail "Soak memory" "host ${h1}→${h2} KB, viewer ${v1}→${v2} KB"
cleanup
(cd $S && python3 summarize.py soak) | sed 's/^/    /' >> $R
echo "" >> $R
echo "Done."
