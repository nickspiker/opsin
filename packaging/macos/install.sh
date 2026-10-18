#!/bin/sh
# Build opsin, wrap it in Opsin.app, and make it the default viewer for everything opsin decodes.
#
# macOS hands a double-clicked file to an app as an Apple Event, and only to a BUNDLE — a bare binary
# on $PATH can never be a Finder handler, however it is installed. Hence this script: the bundle is
# the product, and src/mac_open.rs is the code that catches what the bundle is sent.
#
#   sh packaging/macos/install.sh            → /Applications (falls back to ~/Applications)
#   PREFIX=~/Applications sh …/install.sh    → somewhere else
#   FEATURES=calibrate sh …/install.sh       → with chameleon target-scan calibration
#
# Also installs two Finder Quick Actions (right-click → Quick Actions): "Copy IDT" on one frame, and
# "Paste IDT" on any selection of frames and folders — mismatched cameras ask before pasting.
#
# Re-running is the upgrade path: it rebuilds, replaces the bundle in place and re-claims the types.
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
APP="${PREFIX:-/Applications}/Opsin.app"
BIN="$ROOT/target/release/opsin"
CLI="${CLI_PREFIX:-$HOME/.local/bin}"

echo "==> building"
cd "$ROOT"
cargo build --release ${FEATURES:+--features "$FEATURES"}

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

# A Finder Quick Action is an Automator workflow in ~/Library/Services: one Run Shell Script action, the
# selection passed as arguments ("$@"), all of it in ONE run — so a 300-frame paste is one process and
# one summary. The script calls the bundle's binary, so a re-install upgrades the actions with it.
#   quick_action <menu title> <opsin args> <file type>...
quick_action() {
	TITLE=$1 ARGS=$2
	shift 2
	WF="$HOME/Library/Services/$TITLE.workflow"
	rm -rf "$WF"
	mkdir -p "$WF/Contents"
	TYPES=""
	for T in "$@"; do TYPES="$TYPES<string>$T</string>"; done
	cat >"$WF/Contents/Info.plist" <<-PLIST
	<?xml version="1.0" encoding="UTF-8"?>
	<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
	<plist version="1.0"><dict>
	<key>NSServices</key><array><dict>
	<key>NSMenuItem</key><dict><key>default</key><string>$TITLE</string></dict>
	<key>NSMessage</key><string>runWorkflowAsService</string>
	<key>NSRequiredContext</key><dict><key>NSApplicationIdentifier</key><string>com.apple.finder</string></dict>
	<key>NSSendFileTypes</key><array>$TYPES</array>
	</dict></array>
	</dict></plist>
	PLIST
	cat >"$WF/Contents/document.wflow" <<-PLIST
	<?xml version="1.0" encoding="UTF-8"?>
	<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
	<plist version="1.0"><dict>
	<key>AMApplicationBuild</key><string>523</string>
	<key>AMApplicationVersion</key><string>2.10</string>
	<key>AMDocumentVersion</key><string>2</string>
	<key>actions</key><array><dict><key>action</key><dict>
	<key>AMAccepts</key><dict><key>Container</key><string>List</string><key>Optional</key><true/><key>Types</key><array><string>com.apple.cocoa.string</string></array></dict>
	<key>AMActionVersion</key><string>2.0.3</string>
	<key>AMApplication</key><array><string>Automator</string></array>
	<key>AMParameterProperties</key><dict><key>COMMAND_STRING</key><dict/><key>CheckedForUserDefaultShell</key><dict/><key>inputMethod</key><dict/><key>shell</key><dict/><key>source</key><dict/></dict>
	<key>AMProvides</key><dict><key>Container</key><string>List</string><key>Types</key><array><string>com.apple.cocoa.string</string></array></dict>
	<key>ActionBundlePath</key><string>/System/Library/Automator/Run Shell Script.action</string>
	<key>ActionName</key><string>Run Shell Script</string>
	<key>ActionParameters</key><dict>
	<key>COMMAND_STRING</key><string>exec "$APP/Contents/MacOS/opsin" $ARGS --notify "\$@"</string>
	<key>CheckedForUserDefaultShell</key><true/>
	<key>inputMethod</key><integer>1</integer>
	<key>shell</key><string>/bin/zsh</string>
	<key>source</key><string></string>
	</dict>
	<key>BundleIdentifier</key><string>com.apple.RunShellScript</string>
	<key>CFBundleVersion</key><string>2.0.3</string>
	<key>CanShowSelectedItemsWhenRun</key><false/>
	<key>CanShowWhenRun</key><true/>
	<key>Category</key><array><string>AMCategoryUtilities</string></array>
	<key>Class Name</key><string>RunShellScriptAction</string>
	<key>InputUUID</key><string>$(uuidgen)</string>
	<key>OutputUUID</key><string>$(uuidgen)</string>
	<key>UUID</key><string>$(uuidgen)</string>
	<key>arguments</key><dict/>
	<key>isViewVisible</key><integer>1</integer>
	</dict></dict></array>
	<key>connectors</key><dict/>
	<key>workflowMetaData</key><dict>
	<key>applicationBundleIDsByPath</key><dict><key>/System/Library/CoreServices/Finder.app</key><string>com.apple.finder</string></dict>
	<key>applicationPaths</key><array><string>/System/Library/CoreServices/Finder.app</string></array>
	<key>inputTypeIdentifier</key><string>com.apple.Automator.fileSystemObject</string>
	<key>outputTypeIdentifier</key><string>com.apple.Automator.nothing</string>
	<key>presentationMode</key><integer>15</integer>
	<key>processesInput</key><false/>
	<key>serviceApplicationBundleID</key><string>com.apple.finder</string>
	<key>serviceApplicationPath</key><string>/System/Library/CoreServices/Finder.app</string>
	<key>serviceInputTypeIdentifier</key><string>com.apple.Automator.fileSystemObject</string>
	<key>serviceOutputTypeIdentifier</key><string>com.apple.Automator.nothing</string>
	<key>serviceProcessesInput</key><false/>
	<key>systemImageName</key><string>NSActionTemplate</string>
	<key>useAutomaticInputType</key><false/>
	<key>workflowTypeIdentifier</key><string>com.apple.Automator.servicesMenu</string>
	</dict>
	</dict></plist>
	PLIST
	plutil -lint -s "$WF/Contents/Info.plist" "$WF/Contents/document.wflow"
}

echo "==> installing the Finder Quick Actions"
quick_action "Copy IDT" --copy-idt public.image
quick_action "Paste IDT" --paste-idt public.image public.folder
# The services menu caches what it found; make it look again so the actions appear without a logout.
/System/Library/CoreServices/pbs -update 2>/dev/null || true

echo "==> registering with Launch Services"
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
"$LSREGISTER" -f "$APP"
# Registering only makes opsin ELIGIBLE. Becoming the default is a separate call per content type,
# and there is no command-line tool for it in a stock macOS, so this is the smallest Swift that does it.
swift "$ROOT/packaging/macos/set-default-handler.swift" "$APP/Contents/Info.plist"

echo
echo "opsin installed: $APP"
echo "  cli: $CLI/opsin  (ensure $CLI is on your PATH)"
