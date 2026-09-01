#!/usr/bin/env bash
# T-071: build the amd64 .deb from the release binary.
#
# The version is read from Cargo.toml so the package, the file name and the
# binary can never drift apart. `Depends` names the real runtime floor: the
# shell is built against GTK 4.10 (`gtk4/v4_10`) and the WebKitGTK 6.0 GTK4
# API, so apt refuses the package on a release that cannot satisfy it instead
# of installing something that dies at startup.
set -euo pipefail
cd "$(dirname "$0")/.."

version="$(awk '/^\[workspace\.package\]/ { in_block = 1; next }
                in_block && /^\[/ { exit }
                in_block && /^version[[:space:]]*=/ {
                  gsub(/^version[[:space:]]*=[[:space:]]*"|"[[:space:]]*$/, "")
                  print
                  exit
                }' Cargo.toml)"
[ -n "$version" ] || { echo "could not read version from Cargo.toml" >&2; exit 2; }

cargo build --release -p feathermail

stage="$(mktemp -d)"
trap 'rm -rf -- "$stage"' EXIT
# mktemp gives 0700. Shipping that as the package's root entry would hand the
# installed filesystem root a private mode.
chmod 0755 "$stage"
mkdir -p "$stage/DEBIAN" "$stage/usr/bin" "$stage/usr/share/applications" \
         "$stage/usr/share/metainfo"
install -m755 target/release/feathermail "$stage/usr/bin/feathermail"
install -m644 packaging/app.feathermail.FeatherMail.desktop "$stage/usr/share/applications/"
install -m644 packaging/app.feathermail.FeatherMail.metainfo.xml "$stage/usr/share/metainfo/"

# The canonical icon.png is 1254x1254; a hicolor size directory must hold a
# file of exactly that size, so ship the rescales (packaging/render-icons.sh).
for icon in packaging/icons/hicolor/*/apps/app.feathermail.FeatherMail.png; do
  dir="$stage/usr/share/icons/hicolor/$(basename "$(dirname "$(dirname "$icon")")")/apps"
  mkdir -p "$dir"
  install -m644 "$icon" "$dir/"
done

if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$stage/usr/share/applications/app.feathermail.FeatherMail.desktop"
fi
if command -v appstreamcli >/dev/null; then
  appstreamcli validate "$stage/usr/share/metainfo/app.feathermail.FeatherMail.metainfo.xml" >/dev/null
fi

installed_size="$(du -ks "$stage/usr" | cut -f1)"
sed -e "s/@VERSION@/$version/" -e "s/@INSTALLED_SIZE@/$installed_size/" \
  packaging/debian/control > "$stage/DEBIAN/control"
chmod 0644 "$stage/DEBIAN/control"

mkdir -p target/dist
out="target/dist/feathermail_${version}_amd64.deb"
dpkg-deb --root-owner-group --build "$stage" "$out"
dpkg-deb -I "$out"
echo "T-071: built $out"
