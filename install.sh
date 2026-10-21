#!/bin/sh
# Installs NC-Cable for the current user on any Linux distribution with PipeWire.
# Arch Linux users can use packaging/arch/PKGBUILD instead.
set -eu
cd "$(dirname "$0")"

cargo build --release --locked
install -Dm755 target/release/nc-cable "$HOME/.local/bin/nc-cable"
sed "s|/usr/bin/nc-cable|$HOME/.local/bin/nc-cable|" packaging/systemd/nc-cable.service \
  | install -Dm644 /dev/stdin "$HOME/.config/systemd/user/nc-cable.service"

systemctl --user daemon-reload
systemctl --user enable --now nc-cable.service
echo "NC-Cable installed. Check with: nc-cable status"
