#!/bin/bash
# Build opsin from source and install it — the one command a fresh clone or an upgrade needs.
#
#   ./install.sh                 fetch any missing sibling repos, build, install
#   ./install.sh --prefix DIR    put the binary in DIR (default: where `opsin` already is on PATH, else ~/.local/bin)
#   ./install.sh --features F    build exactly these features instead of the detected ones ("" for none)
#
# Features are detected, so an upgrade never quietly drops one (an install with calibrate alone once lost Live, 2026-11-04):
#   calibrate  when ../chameleon is present (closed source; skipped when it cannot be fetched)
#   live       on Linux (V4L2 camera → virtual webcam)
# The platform script does the rest: packaging/linux/install.sh (binary, file-manager actions, VSF MIME type and
# thumbnailer, desktop entry) or packaging/macos/install.sh (Opsin.app, Finder handler, Quick Actions).
# Siblings are cloned from https://github.com/nickspiker/<name> unless OPSIN_SIBLINGS_BASE names another base URL.

set -e

prefix=""
features=""
features_set=0
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) shift; prefix="${1:-}"; [ -n "$prefix" ] || { echo "install.sh: --prefix needs a directory" >&2; exit 2; } ;;
        --features) shift; features="${1-}"; features_set=1 ;;
        -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "install.sh: unknown option $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

OPSIN="$(cd "$(dirname "$(readlink -f "$0" 2>/dev/null || echo "$0")")" && pwd)"
ROOT="$(cd "$OPSIN/.." && pwd)"
BASE="${OPSIN_SIBLINGS_BASE:-https://github.com/nickspiker}"
cd "$OPSIN"

need() {
    command -v "$1" >/dev/null 2>&1 || { echo "install.sh: $1 is not installed — $2" >&2; exit 1; }
}
need git "install git"
need cargo "install Rust from https://rustup.rs"

# Walk the path dependencies from opsin outward, cloning each sibling that is missing; the list comes from the Cargo.toml path graph, so it never goes stale.
# An optional dependency whose repo cannot be fetched is skipped: optional means the build can go without it (chameleon is one).
echo "Siblings in $ROOT:"
todo="opsin"
seen=""
while [ -n "$todo" ]; do
    cur="${todo%% *}"
    case "$todo" in *" "*) todo="${todo#* }" ;; *) todo="" ;; esac
    case " $seen " in *" $cur "*) continue ;; esac
    seen="$seen $cur"
    while IFS= read -r line; do
        rel="$(printf '%s\n' "$line" | sed -n 's/.*path[[:space:]]*=[[:space:]]*"\.\.\/\([^"/]*\)[^"]*".*/\1/p')"
        [ -n "$rel" ] || continue
        case " $seen $todo " in *" $rel "*) continue ;; esac
        if [ ! -f "$ROOT/$rel/Cargo.toml" ]; then
            if [ -e "$ROOT/$rel" ]; then
                echo "install.sh: $ROOT/$rel exists but holds no Cargo.toml — move it aside and run this again" >&2
                exit 1
            fi
            printf '  %-12s cloning\n' "$rel"
            if ! git clone --quiet "$BASE/$rel" "$ROOT/$rel" 2>/dev/null; then
                case "$line" in
                    *optional*true*) printf '  %-12s skipped (optional, not fetchable)\n' "$rel"; continue ;;
                    *) echo "install.sh: could not clone $BASE/$rel" >&2; exit 1 ;;
                esac
            fi
        fi
        todo="$todo $rel"
    done < <(grep 'path[[:space:]]*=[[:space:]]*"\.\./' "$ROOT/$cur/Cargo.toml")
done
for r in $(printf '%s\n' $seen | sort); do
    [ "$r" = opsin ] || printf '  %-12s %s\n' "$r" "$(git -C "$ROOT/$r" log --oneline -1 2>/dev/null | cut -c1-60)"
done
echo

os="$(uname -s)"
if [ "$features_set" = 0 ]; then
    [ -f "$ROOT/chameleon/Cargo.toml" ] && features="calibrate"
    [ "$os" = Linux ] && features="${features:+$features }live"
fi

# Install over the opsin already on PATH, so the upgraded binary is the one that runs; a build inside a target/ folder is not an install.
if [ -z "$prefix" ]; then
    prefix="${CLI_PREFIX:-}"
fi
if [ -z "$prefix" ]; then
    current="$(command -v opsin 2>/dev/null || true)"
    case "$current" in
        ""|*/target/*) prefix="$HOME/.local/bin" ;;
        *) prefix="$(dirname "$current")" ;;
    esac
fi

echo "Features: ${features:-none}"
echo "Binary:   $prefix/opsin"
echo

case "$os" in
    Linux) FEATURES="$features" CLI_PREFIX="$prefix" sh "$OPSIN/packaging/linux/install.sh" ;;
    Darwin) FEATURES="$features" CLI_PREFIX="$prefix" sh "$OPSIN/packaging/macos/install.sh" ;;
    *) echo "install.sh: no installer for $os — cargo build --release${features:+ --features \"$features\"} builds the binary" >&2; exit 1 ;;
esac

# A running viewer keeps its old binary: a new launch hands its file to that window instead of starting the new one.
if pgrep -x opsin >/dev/null 2>&1; then
    echo
    echo "opsin is running — close every opsin window to start the new build."
fi
