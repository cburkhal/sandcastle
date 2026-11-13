#!/bin/bash
# Build sandcastle.app (and a .dmg) with the CLI inside it.
#
#   packaging/bundle.sh
#
# The .dmg step arranges the window through Finder and needs the terminal to
# have Automation permission for it. `CI=true packaging/bundle.sh` skips that.
#
# Needs the Tauri bundler: cargo install tauri-cli --version "^2" --locked
# Signing and notarization happen when the Apple variables are set (see
# .github/workflows/release.yml); without them the app is unsigned and
# macOS will only open it after a right-click > Open.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TRIPLE="$(rustc -vV | awk '/^host:/{print $2}')"

cd "$ROOT"
cargo build --release --locked -p sandcastle
# Tauri bundles "external binaries" by this naming rule and strips the
# suffix: the CLI ends up as sandcastle.app/Contents/MacOS/sandcastle.
mkdir -p ui/binaries
cp target/release/sandcastle "ui/binaries/sandcastle-$TRIPLE"

cd ui
# The CLI is added here, not in tauri.conf.json, so that a plain
# `cargo build` of the app works without it.
cargo tauri build --config tauri.bundle.conf.json "$@"
echo
echo "Built:"
find "$ROOT/target/release/bundle" -maxdepth 2 \( -name '*.app' -o -name '*.dmg' \) -print
