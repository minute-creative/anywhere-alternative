#!/bin/bash
# The compatibility test: this version against each older release given,
# in both directions (old host + new viewer, new host + old viewer).
# Usage: run-all.sh <new aa-host> <new aa-viewer> <tag>...
set -u
new_host=$1 new_viewer=$2; shift 2
out=${OLD_BUILDS:-old-builds}
dir=$(dirname "$0")
failed=0
"$dir/run-pair.sh" "$new_host" "$new_viewer" "new host + new viewer" input || failed=1
for tag in "$@"; do
  "$dir/run-pair.sh" "$out/$tag/aa-host" "$new_viewer" "$tag host + new viewer" input || failed=1
  "$dir/run-pair.sh" "$new_host" "$out/$tag/aa-viewer" "new host + $tag viewer" || failed=1
done
exit $failed
