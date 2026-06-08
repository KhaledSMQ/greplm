#!/bin/sh
# Install the greplm warm-index daemon as an always-on macOS LaunchAgent.
# Idempotent: re-running reinstalls/reloads. Uninstall is printed at the end.
#
#   contrib/launchd/install-launchd.sh --global         # ONE daemon for all projects (recommended)
#   contrib/launchd/install-launchd.sh [PROJECT_ROOT]   # one daemon for a single project (default: cwd)
#
set -eu

case "$(uname -s)" in
  Darwin) ;;
  *) echo "install-launchd.sh is macOS-only; on Linux see contrib/systemd/." >&2; exit 1 ;;
esac

GREPLM="$(command -v greplm || true)"
[ -n "$GREPLM" ] || { echo "greplm not found on PATH; install it first." >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
AGENTS="$HOME/Library/LaunchAgents"
mkdir -p "$AGENTS"

if [ "${1:-}" = "--global" ]; then
  # One daemon for every project: lazy load + idle eviction over a per-user socket.
  TEMPLATE="$SCRIPT_DIR/com.greplm.global.plist"
  [ -f "$TEMPLATE" ] || { echo "template not found: $TEMPLATE" >&2; exit 1; }
  LABEL="com.greplm.global"
  LOG="$HOME/Library/Logs/greplm-global.log"
  mkdir -p "$(dirname "$LOG")"
  PLIST="$AGENTS/$LABEL.plist"
  sed -e "s|@GREPLM@|$GREPLM|g" -e "s|@LOG@|$LOG|g" "$TEMPLATE" > "$PLIST"
  launchctl unload "$PLIST" 2>/dev/null || true
  launchctl load "$PLIST"
  echo "installed: $LABEL (serves ALL projects)"
  echo "  binary : $GREPLM"
  echo "  logs   : $LOG"
  echo "  plist  : $PLIST"
  echo
  echo "uninstall with:"
  echo "  launchctl unload \"$PLIST\" && rm \"$PLIST\""
  exit 0
fi

# Per-project mode.
ROOT="${1:-$(pwd)}"
ROOT="$(cd "$ROOT" 2>/dev/null && pwd)" || { echo "no such directory: ${1:-$(pwd)}" >&2; exit 1; }
TEMPLATE="$SCRIPT_DIR/com.greplm.daemon.plist"
[ -f "$TEMPLATE" ] || { echo "template not found: $TEMPLATE" >&2; exit 1; }

# Unique, stable label per project root so multiple projects can coexist.
HASH="$(printf '%s' "$ROOT" | shasum | cut -c1-8)"
LABEL="com.greplm.daemon.$HASH"
PLIST="$AGENTS/$LABEL.plist"
mkdir -p "$ROOT/.greplm"

sed -e "s|@LABEL@|$LABEL|g" \
    -e "s|@GREPLM@|$GREPLM|g" \
    -e "s|@ROOT@|$ROOT|g" \
    "$TEMPLATE" > "$PLIST"

launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"

echo "installed: $LABEL"
echo "  serving : $ROOT"
echo "  binary  : $GREPLM"
echo "  logs    : $ROOT/.greplm/daemon.log"
echo "  plist   : $PLIST"
echo
echo "Tip: prefer '--global' to serve every project from one daemon."
echo
echo "uninstall with:"
echo "  launchctl unload \"$PLIST\" && rm \"$PLIST\""
