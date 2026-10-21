#!/bin/sh
# Builds dist/nc-cable_<version>_<arch>.deb inside an Ubuntu container.
# Usage (from the repo root): packaging/debian/build-deb.sh [ubuntu-image]
set -eu
cd "$(dirname "$0")/../.."
image="${1:-ubuntu:24.04}"
mkdir -p dist

docker run --rm -v "$PWD:/src:ro" -v "$PWD/dist:/dist" "$image" sh -euc '
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends \
    ca-certificates curl build-essential pkg-config clang libclang-dev \
    libpipewire-0.3-dev dpkg-dev >/dev/null
  curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal >/dev/null
  . "$HOME/.cargo/env"

  cp -r /src /build && cd /build && rm -rf target
  cargo test --release --locked -q
  cargo build --release --locked

  version=$(sed -n "s/^version = \"\(.*\)\"/\1/p" Cargo.toml | head -1)
  arch=$(dpkg --print-architecture)
  root=/build/deb
  install -Dm755 target/release/nc-cable "$root/usr/bin/nc-cable"
  install -Dm644 packaging/systemd/nc-cable.service "$root/usr/lib/systemd/user/nc-cable.service"
  install -Dm644 README.md "$root/usr/share/doc/nc-cable/README.md"

  # Let dpkg-shlibdeps compute the library dependencies of the binary.
  mkdir -p debian && printf "Source: nc-cable\n\nPackage: nc-cable\nArchitecture: any\n" > debian/control
  shlibs=$(dpkg-shlibdeps -O "$root/usr/bin/nc-cable" | sed "s/^shlibs:Depends=//")

  mkdir -p "$root/DEBIAN"
  cat > "$root/DEBIAN/control" <<CONTROL
Package: nc-cable
Version: $version
Architecture: $arch
Maintainer: Mei <notth4tcold@gmail.com>
Depends: $shlibs, pipewire, pipewire-pulse | pulseaudio-utils, wireplumber
Section: sound
Priority: optional
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Description: Virtual audio cable for PipeWire
 Creates a playback device "NC-Cable Input" and a recording device
 "NC-Cable Output": everything played into the input comes out of the output.
 Enable with: systemctl --user enable --now nc-cable
CONTROL
  dpkg-deb --build --root-owner-group "$root" "/dist/nc-cable_${version}_${arch}.deb"
  chown "$(stat -c %u:%g /dist)" /dist/*.deb
'
