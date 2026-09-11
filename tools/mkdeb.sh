#!/usr/bin/env bash
# Build a Debian/Ubuntu package for OpenLook into dist/.
#
#   ./tools/mkdeb.sh                  # for the machine it runs on
#   ./tools/mkdeb.sh --target jammy   # for Ubuntu 22.04, from any newer host
#
# The package installs:
#   /usr/bin/openlook
#   /usr/share/applications/com.opslogix.Openlook.desktop
#   /usr/bin/openlook-hubspot  plus its manifest under /usr/share/openlook/plugins
#   /usr/share/icons/hicolor/scalable/apps/openlook.svg
#
# For a native build the runtime dependencies are computed from the binary
# with dpkg-shlibdeps. For --target jammy the binary is linked against the
# sysroot fetched by tools/jammy-sysroot.sh and the dependencies are the
# versions that sysroot pinned, since dpkg-shlibdeps would otherwise report
# this host's (newer) package names.
set -euo pipefail

TARGET=native
case "${1:-}" in
  --target) TARGET="${2:?--target needs a value}" ;;
  "") ;;
  *) echo "usage: $0 [--target jammy|native]" >&2; exit 1 ;;
esac

SRC="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$SRC/dist"
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$SRC/Cargo.toml" | head -1)"
ARCH="$(dpkg --print-architecture)"
MAINTAINER="${DEBFULLNAME:-Vincent de Vries} <${DEBEMAIL:-vincent@opslogix.com}>"

for t in dpkg-deb dpkg-shlibdeps; do
  command -v "$t" >/dev/null || { echo "$t not found — sudo apt install dpkg-dev" >&2; exit 1; }
done

# rustup installs here but only adds it to interactive shells' PATH.
[ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "cargo not found — install Rust from https://rustup.rs" >&2; exit 1; }

if [ "$TARGET" = jammy ]; then
  # shellcheck source=tools/jammy-sysroot.sh
  source "$SRC/tools/jammy-sysroot.sh"
  export CARGO_TARGET_DIR="$SRC/target/jammy"
  BUILT="$SRC/target/jammy/release/openlook"
  SUFFIX="ubuntu22.04"
else
  # Same -dev-package fallback as install.sh.
  if ! pkg-config --exists gtk4 libadwaita-1 webkitgtk-6.0 2>/dev/null; then
    echo "GTK development files not found; using tools/dev-shim.sh"
    # shellcheck source=tools/dev-shim.sh
    source "$SRC/tools/dev-shim.sh"
  fi
  BUILT="$SRC/target/release/openlook"
  SUFFIX="$(. /etc/os-release && echo "${ID}${VERSION_ID}")"
fi

echo "Building (release, target: $TARGET)…"
cargo build --release --manifest-path "$SRC/Cargo.toml"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
PKG="$STAGE/pkg"

install -Dm755 "$BUILT"                        "$PKG/usr/bin/openlook"
# Named for the application id: the desktop tells notifications apart by it,
# and drops those it cannot match to an installed application.
install -Dm644 "$SRC/data/com.opslogix.Openlook.desktop" \
    "$PKG/usr/share/applications/com.opslogix.Openlook.desktop"
# Connectors: the program, and the manifest that tells OpenLook about it.
install -Dm755 "$(dirname "$BUILT")/openlook-hubspot" "$PKG/usr/bin/openlook-hubspot"
install -Dm644 "$SRC/data/plugins/hubspot/plugin.json" \
    "$PKG/usr/share/openlook/plugins/hubspot/plugin.json"
install -Dm644 "$SRC/data/openlook.svg"        "$PKG/usr/share/icons/hicolor/scalable/apps/openlook.svg"
install -Dm644 "$SRC/LICENSE"                  "$PKG/usr/share/doc/openlook/copyright"
install -Dm644 "$SRC/README.md"                "$PKG/usr/share/doc/openlook/README.md"

if [ "$TARGET" = jammy ]; then
  # The versions the sysroot linked against; libwebkitgtk-6.0-4 only reaches
  # jammy through jammy-updates/-security, hence the version floor.
  DEPENDS="libc6 (>= 2.34), libgcc-s1 (>= 4.2), libglib2.0-0 (>= 2.72.0), \
libgtk-4-1 (>= 4.6.0), libadwaita-1-0 (>= 1.1.0), libpango-1.0-0 (>= 1.50.0), \
libwebkitgtk-6.0-4 (>= 2.44.0)"
  DEPENDS="$(echo "$DEPENDS" | tr -s ' ')"
else
  # dpkg-shlibdeps insists on a debian/control next to the current directory.
  mkdir -p "$STAGE/debian"
  cat > "$STAGE/debian/control" <<EOF
Source: openlook

Package: openlook
Architecture: $ARCH
Depends: \${shlibs:Depends}
EOF
  DEPENDS="$(cd "$STAGE" && dpkg-shlibdeps -O --ignore-missing-info "$PKG/usr/bin/openlook" 2>/dev/null \
             | sed 's/^shlibs:Depends=//')"
  if [ -z "$DEPENDS" ]; then
    echo "dpkg-shlibdeps found nothing; falling back to a hand-written list" >&2
    DEPENDS="libc6 (>= 2.34), libgtk-4-1 (>= 4.6), libadwaita-1-0 (>= 1.1), libwebkitgtk-6.0-4, libglib2.0-0"
  fi
fi

INSTALLED_KB="$(du -ks "$PKG" | cut -f1)"
mkdir -p "$PKG/DEBIAN"
cat > "$PKG/DEBIAN/control" <<EOF
Package: openlook
Version: $VERSION
Section: mail
Priority: optional
Architecture: $ARCH
Maintainer: $MAINTAINER
Installed-Size: $INSTALLED_KB
Depends: $DEPENDS
Description: Outlook-style native mail client for Linux, with offline mail
 OpenLook is a native GTK4/libadwaita mail client for Microsoft 365
 mailboxes. It keeps a local SQLite copy of your mail so the app opens
 instantly and stays readable offline; changes made offline are queued
 and pushed when the connection returns.
 .
 It offers the classic three-pane layout, several mailboxes at once,
 HTML mail rendering, compose/reply/forward and per-folder search.
EOF

refresh_caches='command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
  command -v gtk-update-icon-cache  >/dev/null && gtk-update-icon-cache -qtf /usr/share/icons/hicolor || true'

cat > "$PKG/DEBIAN/postinst" <<EOF
#!/bin/sh
set -e
if [ "\$1" = configure ]; then
  $refresh_caches
fi
EOF

cat > "$PKG/DEBIAN/postrm" <<EOF
#!/bin/sh
set -e
if [ "\$1" = remove ] || [ "\$1" = purge ]; then
  $refresh_caches
fi
EOF
chmod 755 "$PKG/DEBIAN/postinst" "$PKG/DEBIAN/postrm"

mkdir -p "$DIST"
DEB="$DIST/openlook_${VERSION}-${SUFFIX}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$PKG" "$DEB" >/dev/null

echo
echo "Built $DEB"
dpkg-deb -I "$DEB" | sed -n '/Package:/,/^ /p'
echo
echo "Install it on the target machine with:"
echo "  sudo apt install ./$(basename "$DEB")"
