#!/bin/sh
# Build opsin, wrap it in Opsin.app, and make it the default viewer for everything opsin decodes.
#
# macOS hands a double-clicked file to an app as an Apple Event, and only to a BUNDLE — a bare binary
# on $PATH can never be a Finder handler, however it is installed. Hence this script: the bundle is
# the product, and src/mac_open.rs is the code that catches what the bundle is sent.
#
#   sh packaging/macos/install.sh            → /Applications (falls back to ~/Applications)
#   PREFIX=~/Applications sh …/install.sh    → somewhere else
#
# Re-running is the upgrade path: it rebuilds, replaces the bundle in place and re-claims the types.
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
APP="${PREFIX:-/Applications}/Opsin.app"
BIN="$ROOT/target/release/opsin"
CLI="${CLI_PREFIX:-$HOME/.local/bin}"

echo "==> building"
cd "$ROOT"
cargo build --release

# /Applications is admin-writable on a normal install, but not on every machine.
if ! mkdir -p "$(dirname "$APP")" 2>/dev/null || ! touch "$(dirname "$APP")/.opsin-write-test" 2>/dev/null; then
	APP="$HOME/Applications/Opsin.app"
	echo "==> ${PREFIX:-/Applications} is not writable, using $HOME/Applications"
	mkdir -p "$HOME/Applications"
else
	rm -f "$(dirname "$APP")/.opsin-write-test"
fi

echo "==> building the icon"
ICONSET=$(mktemp -d)/Opsin.iconset
mkdir -p "$ICONSET"
# The hero webp is the app icon; sips cannot read webp directly on every release, so go via PNG.
sips -s format png "$ROOT/opsin.webp" --out "$ICONSET/../orb.png" >/dev/null
for SZ in 16 32 128 256 512; do
	sips -z $SZ $SZ "$ICONSET/../orb.png" --out "$ICONSET/icon_${SZ}x${SZ}.png" >/dev/null
	sips -z $((SZ * 2)) $((SZ * 2)) "$ICONSET/../orb.png" --out "$ICONSET/icon_${SZ}x${SZ}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$ICONSET/../opsin.icns"

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$BIN" "$APP/Contents/MacOS/opsin"
cp "$ICONSET/../opsin.icns" "$APP/Contents/Resources/opsin.icns"
cp "$ROOT/packaging/macos/Info.plist" "$APP/Contents/Info.plist"
printf 'APPL????' >"$APP/Contents/PkgInfo"
# An unsigned Mach-O bundle is refused at launch. Ad-hoc ("-") is enough for a locally built app;
# it is not notarized and will not pass Gatekeeper if copied to another machine.
codesign --force --sign - "$APP"

echo "==> installing the cli to $CLI"
mkdir -p "$CLI"
install -m 755 "$BIN" "$CLI/opsin"

echo "==> registering with Launch Services"
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
"$LSREGISTER" -f "$APP"
# Registering only makes opsin ELIGIBLE. Becoming the default is a separate call per content type,
# and there is no command-line tool for it in a stock macOS, so this is the smallest Swift that does it.
swift "$ROOT/packaging/macos/set-default-handler.swift" "$APP/Contents/Info.plist"

echo
echo "opsin installed: $APP"
echo "  cli: $CLI/opsin  (ensure $CLI is on your PATH)"
