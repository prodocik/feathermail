#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
command -v linuxdeploy >/dev/null || { echo "linuxdeploy is required" >&2; exit 2; }
cargo build --release -p feathermail
appdir="target/AppDir"
rm -rf -- "$appdir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications"
install -m755 target/release/feathermail "$appdir/usr/bin/"
install -m644 packaging/app.feathermail.FeatherMail.desktop "$appdir/usr/share/applications/"
mkdir -p "$appdir/usr/share/metainfo"
install -m644 packaging/app.feathermail.FeatherMail.metainfo.xml "$appdir/usr/share/metainfo/"

# linuxdeploy rejects an icon whose pixel size is not a theme size, and the
# canonical icon.png is 1254x1254. Ship the rescales (packaging/render-icons.sh)
# and hand it the 256x256 one for the AppDir's top-level .DirIcon.
for icon in packaging/icons/hicolor/*/apps/app.feathermail.FeatherMail.png; do
  dir="$appdir/usr/share/icons/hicolor/$(basename "$(dirname "$(dirname "$icon")")")/apps"
  mkdir -p "$dir"
  install -m644 "$icon" "$dir/"
done

version="$(awk '/^\[workspace\.package\]/ { in_block = 1; next }
                in_block && /^\[/ { exit }
                in_block && /^version[[:space:]]*=/ {
                  gsub(/^version[[:space:]]*=[[:space:]]*"|"[[:space:]]*$/, "")
                  print
                  exit
                }' Cargo.toml)"
[ -n "$version" ] || { echo "could not read version from Cargo.toml" >&2; exit 2; }

mkdir -p target/dist
# WebKitGTK stays on the host on purpose. libwebkitgtk-6.0.so.4 spawns
# WebKitWebProcess/WebKitNetworkProcess from a path compiled into the library
# (/usr/lib/x86_64-linux-gnu/webkitgtk-6.0); this build exposes no
# WEBKIT_EXEC_PATH override, so an AppImage cannot redirect it. Bundling the
# library would therefore pair our copy with the host's helper processes — a
# version mismatch away from a dead web process — while buying nothing. The
# AppImage carries GTK4 and everything else and requires libwebkitgtk-6.0-4,
# exactly like the .deb's Depends.
OUTPUT="$(pwd)/target/dist/Feather-Mail-${version}-x86_64.AppImage" \
  linuxdeploy --appdir "$appdir" \
    --desktop-file packaging/app.feathermail.FeatherMail.desktop \
    --icon-file packaging/icons/hicolor/256x256/apps/app.feathermail.FeatherMail.png \
    --exclude-library 'libwebkitgtk-6.0.so*' \
    --exclude-library 'libjavascriptcoregtk-6.0.so*' \
    --output appimage
