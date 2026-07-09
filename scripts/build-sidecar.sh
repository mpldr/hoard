#!/usr/bin/env bash
# Build the `hoard-screen` overlay sidecar from the in-repo crate and drop it
# into the desktop src-tauri root as a Tauri `externalBin`, named with the host
# target triple (+ `.exe` on Windows) exactly as Tauri's bundler expects.
#
# `bundle.externalBin = ["hoard-screen"]` in tauri.conf.json means every desktop
# bundle needs this binary present first — run this before `tauri build`. The
# compiled artifact is gitignored (only the source in `crates/hoard-screen` is
# tracked). Run once per matrix OS in CI, or locally before a desktop build.
#
#   scripts/build-sidecar.sh   (run from anywhere)
set -euo pipefail

HOARD="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DESK="$HOARD/crates/hoard-desktop"
triple="$(rustc -Vv | awk '/^host:/ { print $2 }')"

case "$triple" in
  *windows*)      features="runtime" ;         ext=".exe" ;;
  *apple-darwin*) features="runtime" ;         ext="" ;;
  *)              features="runtime wayland" ; ext="" ;;
esac

echo "Building hoard-screen sidecar ($triple, features: $features)"
cargo build --manifest-path "$HOARD/Cargo.toml" --release -p hoard-screen --features "$features"

# Place the sidecar in the src-tauri root (next to tauri.conf.json), NOT a
# `binaries/` subdir: the bundler flattens externalBin adjacent to the app exe,
# while the shell plugin resolves a sidecar as `exe_dir.join(name)`. A subdir
# prefix in the name makes runtime look for `<exe_dir>/binaries/hoard-screen`,
# which doesn't exist → ENOENT on spawn. externalBin is just `hoard-screen`.
cp "$HOARD/target/release/hoard-screen$ext" "$DESK/hoard-screen-$triple$ext"
echo "Placed sidecar: $DESK/hoard-screen-$triple$ext"

# Next to the desktop exe too, so a plain `cargo build --release` run (no bundle)
# resolves the sidecar at runtime without a full `tauri build`.
if [ -d "$HOARD/target/release" ]; then
  cp "$HOARD/target/release/hoard-screen$ext" "$HOARD/target/release/hoard-screen$ext" 2>/dev/null || true
fi
