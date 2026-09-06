#!/usr/bin/env bash
# One-shot build + systemd setup for redir-rust. Run from the repo root on
# the Linux host that will run the service:
#   ./packaging/systemd/install.sh
#
# Idempotent: safe to re-run after pulling changes (rebuilds the binary,
# refreshes the unit file, leaves an existing config.toml alone).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

bin_name="redir-rust"
install_bin="/usr/local/bin/${bin_name}"
config_dir="/etc/${bin_name}"
config_file="${config_dir}/config.toml"
unit_file="/etc/systemd/system/${bin_name}.service"

echo "==> Building release binary"
cargo build --release

echo "==> Installing binary to ${install_bin}"
sudo install -Dm755 "target/release/${bin_name}" "$install_bin"

echo "==> Ensuring config at ${config_file}"
sudo mkdir -p "$config_dir"
if [[ -f "$config_file" ]]; then
    echo "    already exists, leaving it alone"
else
    sudo cp config.example.toml "$config_file"
    echo "    installed default config.example.toml -- edit ${config_file} before relying on it"
fi

echo "==> Installing unit file to ${unit_file}"
sudo install -Dm644 "packaging/systemd/${bin_name}.service" "$unit_file"

echo "==> Reloading systemd and enabling the service"
sudo systemctl daemon-reload
sudo systemctl enable --now "$bin_name"

echo "==> Done"
sudo systemctl status --no-pager "$bin_name"
