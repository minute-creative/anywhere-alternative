#!/bin/bash
# Build aa-host and aa-viewer for each released version given (v0.5.8 ...)
# into old-builds/<tag>/, skipping versions already there (CI caches it).
#
# In plain words: to check that the new code still talks to older copies of
# Anywhere, we need those older copies. Each is built once from its git tag
# exactly as released (its own Cargo.lock) and kept between CI runs.
set -euo pipefail
out=${OLD_BUILDS:-old-builds}
mkdir -p "$out"
for tag in "$@"; do
  if [ -x "$out/$tag/aa-host" ] && [ -x "$out/$tag/aa-viewer" ]; then
    echo "$tag: already built"
    continue
  fi
  echo "$tag: building"
  src=$(mktemp -d)/src
  git worktree add --detach "$src" "$tag" >/dev/null
  # Old code is built as it was; newer compiler warnings must not stop it.
  RUSTFLAGS="" CARGO_TARGET_DIR="${OLD_TARGET:-$PWD/old-target}" \
    cargo build --locked -q -p aa-host -p aa-viewer --manifest-path "$src/Cargo.toml"
  mkdir -p "$out/$tag"
  cp "${OLD_TARGET:-$PWD/old-target}/debug/aa-host" "${OLD_TARGET:-$PWD/old-target}/debug/aa-viewer" "$out/$tag/"
  strip "$out/$tag/aa-host" "$out/$tag/aa-viewer" 2>/dev/null || true
  git worktree remove --force "$src"
done
