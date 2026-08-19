#!/usr/bin/env bash
# Build the checkout you're in and install it as /Applications/Zeron.app.
#
# Works from any branch and any git worktree: resolves the repo root from your
# current directory (or falls back to this script's tree). Quits the running
# menu-bar app, swaps the bundle, and reopens.
#
# Usage (from anywhere inside a comet/zeron worktree):
#   scripts/install-macos-app.sh
#   scripts/install-macos-app.sh --quick      # binary swap only (faster)
#   scripts/install-macos-app.sh --no-open    # install without launching
#
# Env:
#   ZERON_INSTALL_DIR   default /Applications
#   ZERON_REPO_ROOT     override repo detection
#   ZERON_NO_OPEN=1     same as --no-open

set -euo pipefail

usage() {
  sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
}

QUICK=0
NO_OPEN="${ZERON_NO_OPEN:-0}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --quick) QUICK=1; shift ;;
    --no-open) NO_OPEN=1; shift ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1 (try --help)" >&2
      exit 1
      ;;
  esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: macOS only" >&2
  exit 1
fi

command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"

resolve_root() {
  if [[ -n "${ZERON_REPO_ROOT:-}" ]]; then
    echo "$ZERON_REPO_ROOT"
    return 0
  fi
  local candidate=""
  if candidate="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    if [[ -f "$candidate/apps/zeron/Cargo.toml" ]]; then
      echo "$candidate"
      return 0
    fi
  fi
  local script_root
  script_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  if [[ -f "$script_root/apps/zeron/Cargo.toml" ]]; then
    echo "$script_root"
    return 0
  fi
  return 1
}

ROOT="$(resolve_root)" || {
  echo "error: not inside a zeron/comet checkout (apps/zeron/Cargo.toml missing)" >&2
  echo "  cd into a worktree, or set ZERON_REPO_ROOT" >&2
  exit 1
}

INSTALL_DIR="${ZERON_INSTALL_DIR:-/Applications}"
TARGET="$INSTALL_DIR/Zeron.app"

BRANCH="$(git -C "$ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "?")"
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo "?")"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"

echo "▸ zeron $VERSION — $ROOT"
echo "  branch $BRANCH @ $COMMIT"
echo "  install → $TARGET"

quit_zeron() {
  osascript -e 'quit app "Zeron"' 2>/dev/null || true
  # Give LaunchServices a moment to tear down the menu-bar agent.
  for _ in $(seq 1 20); do
    pgrep -x zeron >/dev/null 2>&1 || return 0
    sleep 0.15
  done
  echo "warning: zeron still running — quit from the menu bar if the swap fails" >&2
}

if [[ "$QUICK" == 1 ]]; then
  if [[ ! -f "$TARGET/Contents/MacOS/zeron" ]]; then
    echo "error: $TARGET not found — run without --quick for a first install" >&2
    exit 1
  fi
  echo "▸ building release binary…"
  (cd "$ROOT" && cargo build --release -p zeron)
  echo "▸ quitting Zeron…"
  quit_zeron
  install -m 755 "$ROOT/target/release/zeron" "$TARGET/Contents/MacOS/zeron"
  codesign --force --sign - "$TARGET"
else
  echo "▸ building Zeron.app (release, no dmg)…"
  ZERON_PACKAGE_APP_ONLY=1 "$ROOT/scripts/package-macos.sh"
  STAGED="$ROOT/target/package/Zeron.app"
  if [[ ! -d "$STAGED" ]]; then
    echo "error: expected bundle at $STAGED" >&2
    exit 1
  fi
  echo "▸ quitting Zeron…"
  quit_zeron
  rm -rf "$TARGET"
  cp -R "$STAGED" "$TARGET"
fi

if [[ "$NO_OPEN" == 1 ]]; then
  echo "▸ installed (not opened — --no-open)"
else
  echo "▸ launching…"
  open "$TARGET"
fi

echo "✓ $TARGET is now $BRANCH @ $COMMIT"
