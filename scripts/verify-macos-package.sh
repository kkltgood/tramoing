#!/usr/bin/env bash
# Read-only verification: never launches or installs the packaged application.
set -euo pipefail
[[ $(uname -s) == Darwin ]] || { echo 'This check requires macOS.' >&2; exit 1; }
DMG=${1:?Pass the DMG path}
SOURCE_APP=${2:?Pass the source application bundle}
SCRIPT_ROOT=$(cd "$(dirname "$0")/.." && pwd)
VERIFY_MOUNT=$(mktemp -d "${TMPDIR:-/tmp}/tramoing-verify.XXXXXX")
cleanup() {
  hdiutil detach "$VERIFY_MOUNT" -quiet || true
  rmdir "$VERIFY_MOUNT" 2>/dev/null || true
}
trap cleanup EXIT
hdiutil verify "$DMG" -quiet
hdiutil attach "$DMG" -readonly -nobrowse -mountpoint "$VERIFY_MOUNT" -quiet
PACKAGED_APP="$VERIFY_MOUNT/Tramoing.app"
codesign --verify --deep --strict "$PACKAGED_APP"
test "$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "$PACKAGED_APP/Contents/Info.plist")" = 'dev.guolongkang.tramoing'
cmp "$SOURCE_APP/Contents/Info.plist" "$PACKAGED_APP/Contents/Info.plist"
cmp "$SOURCE_APP/Contents/MacOS/tramoing" "$PACKAGED_APP/Contents/MacOS/tramoing"
"${DMGBUILD_PYTHON:-python3}" "$SCRIPT_ROOT/packaging/macos/verify-dmg.py" "$VERIFY_MOUNT"
echo 'PASS packaged executable, version information, code signature and disk image integrity'
