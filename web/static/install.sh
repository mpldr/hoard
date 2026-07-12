#!/bin/sh
# Hoard CLI installer — Linux & macOS
#
#   curl -fsSL https://hoard.services/install.sh | sh
#
# Detects your OS/arch, downloads the matching `hoard` tarball from the latest
# GitHub release, verifies its SHA-256, and installs the binary to
# ~/.local/bin (no sudo). Overridable with env vars:
#
#   HOARD_VERSION=1.0.2            pin a version instead of "latest"
#   HOARD_INSTALL_DIR=/opt/bin     install somewhere else
#
# The CLI is the same sync engine as the desktop app, headless. After install:
#   hoard login && hoard sync start
set -eu

REPO="rleeon/hoard"

# ---- pretty output (only when stdout is a tty) -----------------------------
if [ -t 1 ]; then
  BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m'); GREEN=$(printf '\033[32m')
  YELLOW=$(printf '\033[33m'); RED=$(printf '\033[31m'); RESET=$(printf '\033[0m')
else
  BOLD=''; DIM=''; GREEN=''; YELLOW=''; RED=''; RESET=''
fi
say()  { printf '%s\n' "$*"; }
info() { printf '%s==>%s %s\n' "$GREEN" "$RESET" "$*"; }
warn() { printf '%swarning:%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
fail() { printf '%serror:%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

command -v tar >/dev/null 2>&1 || fail "tar is required but not found."

# ---- fetch helper (curl or wget) -------------------------------------------
if command -v curl >/dev/null 2>&1; then
  dl() { curl -fsSL "$1" -o "$2"; }
  dl_stdout() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
  dl() { wget -qO "$2" "$1"; }
  dl_stdout() { wget -qO- "$1"; }
else
  fail "need curl or wget to download."
fi

# ---- detect platform -------------------------------------------------------
os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux)  os=linux ;;
  Darwin) os=macos ;;
  *) fail "unsupported OS: $os (Linux and macOS only; on Windows use install.ps1)." ;;
esac
case "$arch" in
  x86_64|amd64)   arch=x86_64 ;;
  aarch64|arm64)  arch=aarch64 ;;
  *) fail "unsupported architecture: $arch" ;;
esac
if [ "$os" = macos ] && [ "$arch" = x86_64 ]; then
  fail "no Intel-macOS CLI build. Build from source, or self-host the server on Linux."
fi
platform="${os}-${arch}"

# ---- resolve version -------------------------------------------------------
ver="${HOARD_VERSION:-}"
if [ -z "$ver" ]; then
  info "Looking up the latest release…"
  tag=$(dl_stdout "https://api.github.com/repos/$REPO/releases/latest" \
        | grep '"tag_name"' | head -1 \
        | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')
  [ -n "$tag" ] || fail "could not determine the latest version (GitHub API rate limit?). Set HOARD_VERSION."
  ver="${tag#v}"
fi
ver="${ver#v}"

base="https://github.com/$REPO/releases/download/v${ver}"
asset="hoard-${ver}-${platform}.tar.gz"
url="$base/$asset"

# ---- download --------------------------------------------------------------
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t hoard)
trap 'rm -rf "$tmp"' EXIT INT TERM

info "Downloading ${BOLD}${asset}${RESET}"
dl "$url" "$tmp/pkg.tar.gz" || fail "download failed: $url"

# ---- verify sha256 ---------------------------------------------------------
if dl "$url.sha256" "$tmp/pkg.sha256" 2>/dev/null; then
  expected=$(awk '{print $1}' "$tmp/pkg.sha256")
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/pkg.tar.gz" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmp/pkg.tar.gz" | awk '{print $1}')
  else
    actual=''
  fi
  if [ -z "$actual" ]; then
    warn "no sha256 tool found — skipping checksum verification."
  elif [ "$actual" != "$expected" ]; then
    fail "checksum mismatch! expected $expected, got $actual. Aborting."
  else
    info "Checksum verified."
  fi
else
  warn "no .sha256 published for this asset — skipping verification."
fi

# ---- extract ---------------------------------------------------------------
tar -xzf "$tmp/pkg.tar.gz" -C "$tmp"
src="$tmp/hoard-${ver}-${platform}/hoard"
[ -f "$src" ] || fail "the archive did not contain the expected 'hoard' binary."

# ---- install ---------------------------------------------------------------
dir="${HOARD_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$dir"
if command -v install >/dev/null 2>&1; then
  install -m 0755 "$src" "$dir/hoard"
else
  cp "$src" "$dir/hoard" && chmod 0755 "$dir/hoard"
fi
info "Installed ${BOLD}hoard ${ver}${RESET} → ${dir}/hoard"

# ---- PATH check ------------------------------------------------------------
on_path=no
case ":$PATH:" in
  *":$dir:"*) on_path=yes ;;
esac

if [ "$on_path" = no ]; then
  # Pick the rc file for the user's login shell.
  case "${SHELL:-}" in
    */zsh)  rc="$HOME/.zshrc" ;;
    */bash) rc="$HOME/.bashrc" ;;
    *)      rc="$HOME/.profile" ;;
  esac
  line="export PATH=\"$dir:\$PATH\""
  if [ -f "$rc" ] && grep -Fq "$line" "$rc" 2>/dev/null; then
    :
  else
    printf '\n# Added by the Hoard CLI installer\n%s\n' "$line" >> "$rc"
  fi
  say ""
  warn "$dir is not on your PATH yet."
  say "  Added it to ${BOLD}$rc${RESET}. Open a new terminal, or run:"
  say "    ${BOLD}$line${RESET}"
fi

say ""
info "Done. Next steps:"
say "  ${BOLD}hoard login${RESET}       ${DIM}# sign in (Cloud or self-hosted)${RESET}"
say "  ${BOLD}hoard sync start${RESET}  ${DIM}# run the background sync service${RESET}"
say ""
say "${DIM}Docs: https://hoard.services/cli${RESET}"
