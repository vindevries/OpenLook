#!/usr/bin/env bash
# Build OpenLook and install it for the current user (no root needed):
#   - binary       -> ~/.local/bin/openlook
#   - desktop file -> ~/.local/share/applications/openlook.desktop
#   - icon         -> ~/.local/share/icons/hicolor/scalable/apps/openlook.svg
#
# Cached mail lives in ~/.local/share/openlook/ and settings in
# ~/.config/openlook/; neither is touched by installing.
set -euo pipefail

SRC="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$HOME/.local/bin"
APPS_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"

# rustup installs here but only adds it to interactive shells' PATH.
[ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "cargo not found — install Rust from https://rustup.rs" >&2; exit 1; }

# The gtk4/libadwaita/webkit *-dev packages need root. When they are absent,
# fall back to a local pkg-config shim that links against the runtime
# libraries already on the system.
if ! pkg-config --exists gtk4 libadwaita-1 webkitgtk-6.0 2>/dev/null; then
  echo "GTK development files not found; using tools/dev-shim.sh"
  echo "(for a system-standard build: sudo apt install libgtk-4-dev libadwaita-1-dev libwebkitgtk-6.0-dev libgraphene-1.0-dev)"
  # shellcheck source=tools/dev-shim.sh
  source "$SRC/tools/dev-shim.sh"
fi

echo "Building (release)…"
cargo build --release --manifest-path "$SRC/Cargo.toml"

mkdir -p "$BIN_DIR" "$APPS_DIR" "$ICON_DIR"
install -m 755 "$SRC/target/release/openlook" "$BIN_DIR/openlook"
install -m 644 "$SRC/data/openlook.desktop" "$APPS_DIR/openlook.desktop"
install -m 644 "$SRC/data/openlook.svg" "$ICON_DIR/openlook.svg"

# Clean up the payload of the previous Python build, if it is still around.
if [ -d "$HOME/.local/share/openlook/openlook" ]; then
  rm -rf "$HOME/.local/share/openlook/openlook"
  echo "Removed the old Python program files (cached mail was left alone)."
fi

command -v update-desktop-database >/dev/null && update-desktop-database "$APPS_DIR" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

echo "Installed. Launch with 'openlook' or from the Activities overview."
echo "(Make sure ~/.local/bin is on your PATH — it is by default on Ubuntu.)"
