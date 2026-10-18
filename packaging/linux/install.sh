#!/bin/sh
# Build opsin, install the CLI, and add "Copy IDT" / "Paste IDT" to the file managers' right-click menus.
#
# On Linux the binary on $PATH is the whole app, so the work here is the menus. Each file manager has
# its own extension format; this writes all of them (an absent file manager just never reads its file):
#   Dolphin (KDE)   ~/.local/share/kio/servicemenus/opsin-idt.desktop   (+ the KF5 location)
#   Nautilus (GNOME) ~/.local/share/nautilus/scripts/{Copy,Paste} IDT     → right-click → Scripts
#   Nemo (Cinnamon)  ~/.local/share/nemo/actions/opsin-*-idt.nemo_action
#   Caja (MATE)      ~/.config/caja/scripts/{Copy,Paste} IDT              → right-click → Scripts
# Every one passes the whole selection in ONE run, so a 300-frame paste is one process and one summary.
# Results come back thru notify-send; failures and camera mismatches thru zenity or kdialog.
#
#   sh packaging/linux/install.sh                   → ~/.local/bin/opsin
#   FEATURES=calibrate sh …/install.sh              → with chameleon target-scan calibration
#   CLI_PREFIX=/usr/local/bin sh …/install.sh       → somewhere else
#
# Re-running is the upgrade path.
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CLI="${CLI_PREFIX:-$HOME/.local/bin}"
BIN="$CLI/opsin"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONF="${XDG_CONFIG_HOME:-$HOME/.config}"

echo "==> building"
cd "$ROOT"
cargo build --release ${FEATURES:+--features "$FEATURES"}

echo "==> installing the cli to $CLI"
mkdir -p "$CLI"
install -m 755 "$ROOT/target/release/opsin" "$BIN"

echo "==> installing the file-manager actions"
# Dolphin: one service menu, two actions. Copy wants exactly one frame; Paste takes files and folders (opsin
# itself skips what can't carry an IDT, so the menu need not know every raw MIME type). KF6 runs a service
# menu only if it is executable.
for DIR in "$DATA/kio/servicemenus" "$DATA/kservices5/ServiceMenus"; do
	mkdir -p "$DIR"
	cat >"$DIR/opsin-idt.desktop" <<-DESKTOP
	[Desktop Entry]
	Type=Service
	MimeType=all/allfiles;inode/directory;
	Actions=copy_idt;paste_idt;
	X-KDE-Submenu=opsin
	X-KDE-Priority=TopLevel

	[Desktop Action copy_idt]
	Name=Copy IDT
	Icon=edit-copy
	X-KDE-RequiredNumberOfUrls=1
	Exec="$BIN" --copy-idt --notify %F

	[Desktop Action paste_idt]
	Name=Paste IDT
	Icon=edit-paste
	Exec="$BIN" --paste-idt --notify %F
	DESKTOP
	chmod 755 "$DIR/opsin-idt.desktop"
done

# Nautilus and Caja: a script is the menu item, named by its file name, with the selection as arguments.
for DIR in "$DATA/nautilus/scripts" "$CONF/caja/scripts"; do
	mkdir -p "$DIR"
	printf '#!/bin/sh\nexec "%s" --copy-idt --notify "$@"\n' "$BIN" >"$DIR/Copy IDT"
	printf '#!/bin/sh\nexec "%s" --paste-idt --notify "$@"\n' "$BIN" >"$DIR/Paste IDT"
	chmod 755 "$DIR/Copy IDT" "$DIR/Paste IDT"
done

# Nemo: Selection=s is "exactly one", notnone is "one or more"; Extensions=any includes folders.
mkdir -p "$DATA/nemo/actions"
cat >"$DATA/nemo/actions/opsin-copy-idt.nemo_action" <<-NEMO
[Nemo Action]
Name=Copy IDT
Comment=Copy this frame's IDT for pasting onto others
Exec="$BIN" --copy-idt --notify %F
Icon-Name=edit-copy
Selection=s
Extensions=any;
NEMO
cat >"$DATA/nemo/actions/opsin-paste-idt.nemo_action" <<-NEMO
[Nemo Action]
Name=Paste IDT
Comment=Paste the copied IDT onto these frames (and the frames in these folders)
Exec="$BIN" --paste-idt --notify %F
Icon-Name=edit-paste
Selection=notnone
Extensions=any;
NEMO

echo
echo "opsin installed: $BIN  (ensure $CLI is on your PATH)"
echo "  right-click → Copy IDT / Paste IDT in Dolphin, Nemo; → Scripts in Nautilus and Caja"
command -v zenity >/dev/null 2>&1 || command -v kdialog >/dev/null 2>&1 || echo "  note: install zenity or kdialog to be asked before pasting onto a different camera (without one, mismatches are skipped)"
