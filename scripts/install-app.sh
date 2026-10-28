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

# Ad-hoc re-sign with a stable identifier. The bundler otherwise derives the
# code identity from the build (project_s-<hash>), and macOS privacy grants —
# including «Локальная сеть» — are keyed to that identity, so they are lost on
# every reinstall. `app.project-s.ru` matches CFBundleIdentifier.
if command -v codesign >/dev/null 2>&1; then
  if codesign --force --sign - --identifier app.project-s.ru "$DEST" 2>/dev/null; then
    echo "Signed ad-hoc as app.project-s.ru"
  else
    echo "WARNING: ad-hoc re-sign failed; local network permission may reset" >&2
  fi
fi

echo "Installed: $DEST"
du -sh "$DEST"
