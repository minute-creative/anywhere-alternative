#!/bin/bash
name=$1 res=$2 fps=$3 relay=$4 secs=${5:-12}
B=${BIN_DIR:-$(cd "$(dirname "$0")/../.." && pwd)/target/release}
L=/tmp/claude-0/sim/$name
$B/aa-host --mock --mock-res $res --mock-fps $fps --listen 127.0.0.1:7700 > $L.host.log 2>&1 &
HP=$!
target=127.0.0.1:7700; RP=
if [ "$relay" != none ]; then python3 $S/relay.py $relay > $L.relay.log 2>&1 & RP=$!; target=127.0.0.1:7800; fi
sleep 0.7
timeout $secs $B/aa-viewer --headless --mock $target --bind 127.0.0.1:0 > $L.viewer.log 2>&1
kill $HP $RP 2>/dev/null; wait 2>/dev/null; true
