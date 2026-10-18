#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUNDLE="$REPO/src-tauri/target/release/bundle/macos/Project-S.app"
DEST="/Applications/Project-S.app"

pkill -f "Project-S.app/Contents/MacOS" 2>/dev/null || true
pkill -f "Project S.app/Contents/MacOS" 2>/dev/null || true

if [ ! -d "$BUNDLE" ]; then
  echo "ERROR: built bundle not found at: $BUNDLE" >&2
  echo "Run 'npm run tauri build' first." >&2
  exit 1
fi

rm -rf "/Applications/Project S.app" "$DEST"
ditto "$BUNDLE" "$DEST"
xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true

echo "Installed: $DEST"
du -sh "$DEST"
