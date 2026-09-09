#!/usr/bin/env bash
# Fetch Ubuntu 22.04 (jammy) runtime libraries into a local sysroot and point
# the build at them, so a jammy-compatible binary can be produced from a newer
# machine without root, a container or a VM.
#
# Nothing but the linker sees this: the .so files are unpacked under
# target/sysroot-jammy/, and the binary that comes out records the normal
# SONAMEs (libgtk-4.so.1 etc.).
#
# Usage:  source tools/jammy-sysroot.sh   (then cargo build)
set -u

_sys_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/sysroot-jammy"
_pool="$_sys_root/pool"
_lib="$_sys_root/lib"
_pc="$_sys_root/pkgconfig"
_mirror="http://archive.ubuntu.com/ubuntu"

# Runtime packages the linker needs to resolve every -l flag below.
_packages=(
  libglib2.0-0 libcairo2 libcairo-gobject2 libpango-1.0-0 libpangocairo-1.0-0
  libgdk-pixbuf-2.0-0 libgraphene-1.0-0 libgtk-4-1 libadwaita-1-0
  libwebkitgtk-6.0-4 libjavascriptcoregtk-6.0-1 libsoup-3.0-0
)

mkdir -p "$_pool" "$_lib" "$_pc"

# ---- resolve package -> pool URL from the jammy indices --------------------
_index="$_sys_root/Packages.txt"
if [ ! -s "$_index" ]; then
  echo "jammy-sysroot: fetching package indices…"
  : > "$_index"
  for suite in jammy jammy-updates jammy-security; do
    for comp in main universe; do
      curl -fsSL "$_mirror/dists/$suite/$comp/binary-amd64/Packages.gz" \
        | gzip -d >> "$_index" 2>/dev/null || true
    done
  done
fi
[ -s "$_index" ] || { echo "jammy-sysroot: could not fetch package indices" >&2; return 1; }

# Latest version of a package wins, so jammy-updates beats jammy.
_filename_of() { # package -> pool path
  awk -v want="$1" '
    /^Package: / { pkg = $2 }
    /^Version: / { ver = $2 }
    /^Filename: / { if (pkg == want) print ver, $2 }
  ' "$_index" | sort -V | tail -1 | awk '{print $2}'
}

for pkg in "${_packages[@]}"; do
  stamp="$_pool/.$pkg.done"
  [ -e "$stamp" ] && continue
  path="$(_filename_of "$pkg")"
  [ -n "$path" ] || { echo "jammy-sysroot: $pkg not found in the jammy indices" >&2; return 1; }
  echo "jammy-sysroot: $pkg  ($(basename "$path"))"
  curl -fsSL "$_mirror/$path" -o "$_pool/$pkg.deb" || return 1
  dpkg-deb -x "$_pool/$pkg.deb" "$_pool/root" || return 1
  touch "$stamp"
done

# ---- unversioned symlinks so -lfoo resolves --------------------------------
_jlib="$_pool/root/usr/lib/x86_64-linux-gnu"
_link() { # -l name  ->  soname glob
  local stem="$1" glob="$2" actual
  actual="$(ls -1 $_jlib/$glob 2>/dev/null | sort -V | tail -1)"
  [ -n "$actual" ] || { echo "jammy-sysroot: missing $glob" >&2; return 1; }
  ln -sf "$actual" "$_lib/lib$stem.so"
}
_link glib-2.0            'libglib-2.0.so.0*'
_link gobject-2.0         'libgobject-2.0.so.0*'
_link gio-2.0             'libgio-2.0.so.0*'
_link gmodule-2.0         'libgmodule-2.0.so.0*'
_link cairo               'libcairo.so.2*'
_link cairo-gobject       'libcairo-gobject.so.2*'
_link pango-1.0           'libpango-1.0.so.0*'
_link pangocairo-1.0      'libpangocairo-1.0.so.0*'
_link gdk_pixbuf-2.0      'libgdk_pixbuf-2.0.so.0*'
_link graphene-1.0        'libgraphene-1.0.so.0*'
_link gtk-4               'libgtk-4.so.1*'
_link adwaita-1           'libadwaita-1.so.0*'
_link webkitgtk-6.0       'libwebkitgtk-6.0.so.4*'
_link javascriptcoregtk-6.0 'libjavascriptcoregtk-6.0.so.1*'
_link soup-3.0            'libsoup-3.0.so.0*'

# ---- pkg-config metadata pointing at the sysroot ---------------------------
_emit() { # name version libs [requires]
  cat > "$_pc/$1.pc" <<EOF
prefix=/usr
libdir=$_lib
includedir=/usr/include

Name: $1
Description: jammy sysroot shim for $1
Version: $2
Requires: ${4:-}
Libs: -L\${libdir} $3
Cflags:
EOF
}

# Versions as jammy ships them; the -sys crates check these against the
# feature levels in Cargo.toml.
_emit glib-2.0              2.72.4  "-lglib-2.0"
_emit gobject-2.0           2.72.4  "-lgobject-2.0"       "glib-2.0"
_emit gio-2.0               2.72.4  "-lgio-2.0"           "glib-2.0 gobject-2.0"
_emit gmodule-2.0           2.72.4  "-lgmodule-2.0"       "glib-2.0"
_emit gmodule-no-export-2.0 2.72.4  "-lgmodule-2.0"       "glib-2.0"
_emit cairo                 1.16.0  "-lcairo"
_emit cairo-gobject         1.16.0  "-lcairo-gobject"     "cairo glib-2.0 gobject-2.0"
_emit pango                 1.50.6  "-lpango-1.0"         "glib-2.0 gobject-2.0"
_emit pangocairo            1.50.6  "-lpangocairo-1.0"    "pango cairo"
_emit gdk-pixbuf-2.0        2.42.8  "-lgdk_pixbuf-2.0"    "glib-2.0 gobject-2.0 gio-2.0"
_emit graphene-1.0          1.10.8  "-lgraphene-1.0"      "glib-2.0 gobject-2.0"
_emit graphene-gobject-1.0  1.10.8  "-lgraphene-1.0"      "glib-2.0 gobject-2.0"
_emit gtk4                  4.6.9   "-lgtk-4" \
  "glib-2.0 gobject-2.0 gio-2.0 cairo cairo-gobject pango pangocairo gdk-pixbuf-2.0 graphene-gobject-1.0"
_emit libadwaita-1          1.1.7   "-ladwaita-1"         "gtk4 gio-2.0"
_emit javascriptcoregtk-6.0 2.50.4  "-ljavascriptcoregtk-6.0" "glib-2.0 gobject-2.0"
_emit libsoup-3.0           3.0.7   "-lsoup-3.0"          "glib-2.0 gobject-2.0 gio-2.0"
_emit webkitgtk-6.0         2.50.4  "-lwebkitgtk-6.0"     "gtk4 javascriptcoregtk-6.0 gio-2.0 libsoup-3.0"

# PKG_CONFIG_LIBDIR (not _PATH) so the host's own .pc files are ignored
# entirely — otherwise the build would silently link against noble.
export PKG_CONFIG_LIBDIR="$_pc"
export RUSTFLAGS="-L $_lib${RUSTFLAGS:+ $RUSTFLAGS}"
export OPENLOOK_SYSROOT="$_sys_root"

echo "jammy-sysroot: gtk4 $(pkg-config --modversion gtk4), libadwaita-1 $(pkg-config --modversion libadwaita-1), webkitgtk-6.0 $(pkg-config --modversion webkitgtk-6.0)"
