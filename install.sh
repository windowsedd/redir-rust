#!/usr/bin/env bash
# redir-rust release installer for Linux.
#
# Downloads a published release binary, verifies its sha256, installs it, and
# (unless --no-service) hands off to the binary's own --install-systemd to set
# up /etc/redir-rust/config.toml and the systemd unit. No Rust toolchain and
# no repo checkout needed.
#
#   curl -fsSL https://raw.githubusercontent.com/windowsedd/redir-rust/main/install.sh | sudo bash
#
# Options (also settable as env vars):
#   --version <tag>    REDIR_VERSION    release tag, e.g. v0.1.0 (default: latest)
#   --target <triple>  REDIR_TARGET     e.g. x86_64-unknown-linux-gnu
#                                       (default: x86_64-unknown-linux-musl, static)
#   --bin-dir <dir>    REDIR_BIN_DIR    install location (default: /usr/local/bin)
#   --no-service       REDIR_NO_SERVICE=1   install the binary only, no systemd setup
#
# To build from a checkout instead, see packaging/systemd/install.sh.
set -euo pipefail

REPO="windowsedd/redir-rust"
BIN_NAME="redir-rust"

version="${REDIR_VERSION:-}"
target="${REDIR_TARGET:-x86_64-unknown-linux-musl}"
bin_dir="${REDIR_BIN_DIR:-/usr/local/bin}"
no_service="${REDIR_NO_SERVICE:-}"

die() {
    echo "error: $*" >&2
    exit 1
}

info() { echo "==> $*"; }

# Prints the comment header above, minus the shebang, as --help text.
usage() {
    awk 'NR > 1 { if ($0 ~ /^#/) { sub(/^# ?/, ""); print } else exit }' "$0"
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) version="${2:-}"; shift 2 ;;
        --target) target="${2:-}"; shift 2 ;;
        --bin-dir) bin_dir="${2:-}"; shift 2 ;;
        --no-service) no_service=1; shift ;;
        -h | --help) usage ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

[[ "$(uname -s)" == "Linux" ]] || die "this installer is Linux-only (found $(uname -s))"

# The release workflow only publishes x86_64 archives; anything else has to
# build from source rather than silently getting the wrong binary.
arch="$(uname -m)"
case "$arch" in
    x86_64 | amd64) ;;
    *) die "no published release binary for $arch; build from source (see packaging/systemd/install.sh)" ;;
esac

for tool in tar install mktemp; do
    command -v "$tool" > /dev/null || die "required tool not found: $tool"
done

if command -v curl > /dev/null; then
    fetch() { curl -fsSL "$1" -o "$2"; }
    fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget > /dev/null; then
    fetch() { wget -qO "$2" "$1"; }
    fetch_stdout() { wget -qO - "$1"; }
else
    die "need curl or wget to download the release"
fi

# Root is needed for /usr/local/bin and /etc; re-exec through sudo rather than
# failing halfway through the install.
SUDO=""
if [[ "$(id -u)" -ne 0 ]]; then
    command -v sudo > /dev/null || die "must run as root (no sudo available)"
    SUDO="sudo"
fi

if [[ -z "$version" ]]; then
    info "Resolving latest release"
    version="$(fetch_stdout "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n1)"
    [[ -n "$version" ]] || die "could not resolve the latest release tag; pass --version <tag>"
fi

archive="${BIN_NAME}-${version}-${target}.tar.gz"
base_url="https://github.com/${REPO}/releases/download/${version}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

info "Downloading ${archive} (${version})"
fetch "${base_url}/${archive}" "${tmp}/${archive}" \
    || die "download failed: ${base_url}/${archive}"

if command -v sha256sum > /dev/null && fetch "${base_url}/${archive}.sha256" "${tmp}/${archive}.sha256" 2> /dev/null; then
    info "Verifying sha256"
    (cd "$tmp" && sha256sum -c "${archive}.sha256" > /dev/null) || die "sha256 mismatch for ${archive}"
else
    echo "warning: skipping sha256 verification (no sha256sum, or no published sum)" >&2
fi

tar -xzf "${tmp}/${archive}" -C "$tmp"
binary="${tmp}/${BIN_NAME}-${version}-${target}/${BIN_NAME}"
[[ -x "$binary" ]] || die "archive did not contain ${BIN_NAME}"

# Install via a temp file + rename: a plain overwrite of a binary that is
# currently running as the service fails with "Text file busy".
info "Installing to ${bin_dir}/${BIN_NAME}"
$SUDO mkdir -p "$bin_dir"
$SUDO install -m755 "$binary" "${bin_dir}/.${BIN_NAME}.new"
$SUDO mv -f "${bin_dir}/.${BIN_NAME}.new" "${bin_dir}/${BIN_NAME}"

if [[ -n "$no_service" ]]; then
    info "Skipping systemd setup (--no-service)"
elif ! command -v systemctl > /dev/null || [[ ! -d /run/systemd/system ]]; then
    echo "warning: systemd not detected; installed the binary only" >&2
else
    # The binary embeds the unit file and the default config, so it can finish
    # the install itself — one source of truth for both install paths.
    info "Setting up the systemd service"
    $SUDO "${bin_dir}/${BIN_NAME}" --install-systemd
fi

info "Installed $("${bin_dir}/${BIN_NAME}" -V)"
cat <<EOF

Next steps:
  sudo ${BIN_NAME} -e            # edit /etc/redir-rust/config.toml
  sudo ${BIN_NAME} --restart     # apply the config
  sudo ${BIN_NAME} --service-status
EOF
