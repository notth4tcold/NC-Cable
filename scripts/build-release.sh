#!/bin/sh
# Builds the release artifacts into dist/:
#   nc-cable-<ver>-1-x86_64.pkg.tar.zst  (Arch Linux)
#   nc-cable_<ver>_amd64.deb             (Ubuntu 24.04+ / Debian 13+)
set -eu
cd "$(dirname "$0")/.."
mkdir -p dist

(cd packaging/arch && makepkg -f --noconfirm "$@" && rm -rf src pkg)
mv packaging/arch/nc-cable-[0-9]*.pkg.tar.zst dist/
rm -f packaging/arch/nc-cable-debug-*.pkg.tar.zst

packaging/debian/build-deb.sh

(cd dist && sha256sum ./*.pkg.tar.zst ./*.deb > SHA256SUMS)
ls -l dist
