#!/usr/bin/env bash
# Build from this checkout and use the binary's systemd installer.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

cargo build --release --locked
sudo ./target/release/redir-rust --install-systemd
