#!/bin/bash
# Publish opsin to crates.io without the `calibrate` feature. Calibration runs thru chameleon, which is closed source and not on crates.io, and Cargo writes every optional dependency into the published manifest and requires it to exist there. So this rewrites Cargo.toml for the publish only — the chameleon dependency and the `calibrate` feature removed, the feature declared to rustc's cfg check so the gated code builds quietly without it — and always restores the original Cargo.toml and Cargo.lock, success or failure. Local builds keep `--features calibrate` unchanged.
# Usage: scripts/publish.sh [cargo publish args, e.g. --dry-run]
set -euo pipefail
cd "$(dirname "$0")/.."
bak=$(mktemp -d)
cp Cargo.toml Cargo.lock "$bak"/
trap 'cp "$bak"/Cargo.toml "$bak"/Cargo.lock .; rm -rf "$bak"' EXIT
sed -i -e '/^chameleon = /d' -e '/^calibrate = \["dep:chameleon"\]/d' Cargo.toml
if grep -E '^[^#]*chameleon' Cargo.toml >/dev/null; then
    echo "publish.sh: Cargo.toml still names chameleon outside a comment; refusing to publish" >&2
    exit 1
fi
cat >> Cargo.toml <<'TOML'

[lints.rust]
unexpected_cfgs = { level = "warn", check-cfg = ['cfg(feature, values("calibrate"))'] }
TOML
cargo publish --allow-dirty "$@"
