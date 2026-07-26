#!/usr/bin/env bash
# Build the desktop's sidecar binaries from the in-repo crates and drop them into
# the desktop src-tauri root as Tauri `externalBin`s, named with the host target
# triple (+ `.exe` on Windows) exactly as Tauri's bundler expects.
#
# Two of them:
#   - `hoard-screen` — the in-game overlay (Pro layer).
#   - `hoardd`       — the local sync service that owns the engine (ADR 0021).
#                      The desktop is a thin client of it and starts it when it's
#                      absent, so a bundle without `hoardd` is an app that can't
#                      sync at all.
#
# `bundle.externalBin` in tauri.conf.json lists both, which means every desktop
# bundle needs them present first — run this before `tauri build`. The compiled
# artifacts are gitignored (only the sources under `crates/` are tracked). Run
# once per matrix OS in CI, or locally before a desktop build.
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

# Place each sidecar in the src-tauri root (next to tauri.conf.json), NOT a
# `binaries/` subdir: the bundler flattens externalBin adjacent to the app exe,
# while the shell plugin resolves a sidecar as `exe_dir.join(name)`. A subdir
# prefix in the name makes runtime look for `<exe_dir>/binaries/<name>`, which
# doesn't exist → ENOENT on spawn. externalBin is just the bare name.
#
# A plain `cargo build --release` (no bundle) needs no copy at all: both binaries
# already land in `target/release`, next to `hoard-desktop`, which is the first
# place the sidecar lookup and `hoardd::client::daemon_binary` look.
# `install -m 0755`, no `cp`, y el bit de ejecución se comprueba después.
#
# El bundler mete el sidecar en el paquete **con el modo que tenga aquí**, así que
# un fichero sin permiso de ejecución se instala sin él y la app no puede lanzar
# el servicio: `.deb` que instala bien, app que no sincroniza y un ENOENT/EACCES
# que no lleva a ninguna parte. Pasó de verdad (2026-07-26): el `cp` conserva el
# modo del **destino** cuando ya existe, así que un sidecar que perdiera el bit
# una vez se quedaba sin él en todas las builds siguientes de esa copia de
# trabajo. `install` fija el modo siempre, y el chequeo convierte un fallo
# silencioso de empaquetado en un build roto.
place() {
  local name="$1"
  local dest="$DESK/$name-$triple$ext"
  install -m 0755 "$HOARD/target/release/$name$ext" "$dest"
  [ -x "$dest" ] || { echo "ERROR: $dest isn't executable" >&2; exit 1; }
  echo "Placed sidecar: $dest"
}

echo "Building hoard-screen sidecar ($triple, features: $features)"
cargo build --manifest-path "$HOARD/Cargo.toml" --release -p hoard-screen --features "$features"
place hoard-screen

# The sync service. It's also what the per-user autostart unit execs, so the copy
# that ships next to the app is the one the OS runs at login — the client
# resolves the daemon as its own sibling first and only then falls back to PATH.
echo "Building hoardd sidecar ($triple)"
cargo build --manifest-path "$HOARD/Cargo.toml" --release -p hoardd
place hoardd
