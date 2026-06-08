#!/bin/sh
# Install the greplm warm-index daemon as an always-on macOS LaunchAgent for one
# project. Idempotent: re-running reinstalls/reloads. Usage:
#
#   contrib/launchd/install-launchd.sh [PROJECT_ROOT]   # default: current dir
#
# Uninstall is printed at the end.
set -eu

case "$(uname -s)" in
  Darwin) ;;
  *) echo "install-launchd.sh is macOS-only; on Linux see contrib/systemd/." >&2; exit 1 ;;
esac

ROOT="${1:-$(pwd)}"
ROOT="$(cd "$ROOT" 2>/dev/null && pwd)" || { echo "no such directory: ${1:-$(pwd)}" >&2; exit 1; }

GREPLM="$(command -v greplm || true)"
[ -n "$GREPLM" ] || { echo "greplm not found on PATH; install it first." >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TEMPLATE="$SCRIPT_DIR/com.greplm.daemon.plist"
[ -f "$TEMPLATE" ] || { echo "template not found: $TEMPLATE" >&2; exit 1; }

# Unique, stable label per project root so multiple projects can coexist.
HASH="$(printf '%s' "$ROOT" | shasum | cut -c1-8)"
LABEL="com.greplm.daemon.$HASH"
AGENTS="$HOME/Library/LaunchAgents"
PLIST="$AGENTS/$LABEL.plist"

mkdir -p "$AGENTS" "$ROOT/.greplm"

# Fill the template. '|' is the sed delimiter; paths won't contain it.
sed -e "s|@LABEL@|$LABEL|g" \
    -e "s|@GREPLM@|$GREPLM|g" \
    -e "s|@ROOT@|$ROOT|g" \
    "$TEMPLATE" > "$PLIST"

# Reload cleanly (ignore "not loaded" on first install).
launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"

echo "installed: $LABEL"
echo "  serving : $ROOT"
echo "  binary  : $GREPLM"
echo "  logs    : $ROOT/.greplm/daemon.log"
echo "  plist   : $PLIST"
echo
echo "uninstall with:"
echo "  launchctl unload \"$PLIST\" && rm \"$PLIST\""
