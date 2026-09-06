#!/usr/bin/env bash
# redir-rust release updater for Linux.
#
# Compares the installed binary's version against the latest published
# release, and if it's behind, installs the new one and restarts the systemd
# unit. Does nothing when already current, so it's safe to run from cron.
#
# Note that the restart is a hard cut: every open connection is dropped. On a
# game relay that means kicking whoever is online, so an unattended schedule
# wants an off-peak window (or --no-restart, and bounce the service yourself).
#
#   curl -fsSL https://raw.githubusercontent.com/windowsedd/redir-rust/main/update.sh | sudo bash
#
# Options (also settable as env vars):
#   --version <tag>    REDIR_VERSION    update to this tag instead of the latest
#   --target <triple>  REDIR_TARGET     e.g. x86_64-unknown-linux-gnu
#                                       (default: x86_64-unknown-linux-musl, static)
#   --bin-dir <dir>    REDIR_BIN_DIR    where the binary lives (default: /usr/local/bin)
#   --unit <name>      REDIR_UNIT       unit to restart (default: redir-rust.service)
#   --check            report what an update would do, change nothing
#   --force            reinstall even when the versions already match
#   --no-restart       install the new binary but leave the running service alone
#
# Exit codes: 0 = up to date or updated, 10 = --check found an update, 1 = error.
# For a first-time install (or to build from a checkout), see install.sh and
# packaging/systemd/install.sh.
set -euo pipefail

REPO="windowsedd/redir-rust"
BIN_NAME="redir-rust"
RAW_INSTALLER="https://raw.githubusercontent.com/${REPO}/main/install.sh"

version="${REDIR_VERSION:-}"
target="${REDIR_TARGET:-x86_64-unknown-linux-musl}"
bin_dir="${REDIR_BIN_DIR:-/usr/local/bin}"
unit="${REDIR_UNIT:-redir-rust.service}"
check_only=""
force=""
no_restart=""

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
        --unit) unit="${2:-}"; shift 2 ;;
        --check) check_only=1; shift ;;
        --force) force=1; shift ;;
        --no-restart) no_restart=1; shift ;;
        -h | --help) usage ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

[[ "$(uname -s)" == "Linux" ]] || die "this updater is Linux-only (found $(uname -s))"

if command -v curl > /dev/null; then
    fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget > /dev/null; then
    fetch_stdout() { wget -qO - "$1"; }
else
    die "need curl or wget to check for updates"
fi

installed="${bin_dir}/${BIN_NAME}"
if [[ ! -x "$installed" ]]; then
    # Fall back to $PATH so a non-default --bin-dir install is still found.
    installed="$(command -v "$BIN_NAME" 2> /dev/null || true)"
    [[ -n "$installed" ]] || die "no ${BIN_NAME} installed in ${bin_dir} or on \$PATH; use install.sh first"
fi

# `redir-rust -V` prints "redir-rust 0.1.0 (<commit> <profile>)".
current="$("$installed" -V | awk '{print $2}')"
[[ -n "$current" ]] || die "could not read the installed version from ${installed} -V"

if [[ -z "$version" ]]; then
    info "Checking for updates (installed: ${current})"
    version="$(fetch_stdout "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n1)"
    [[ -n "$version" ]] || die "could not resolve the latest release tag; pass --version <tag>"
fi

# Tags are v-prefixed; the binary reports a bare Cargo version.
latest="${version#v}"

if [[ "$current" == "$latest" && -z "$force" ]]; then
    echo "${BIN_NAME} ${current} is already up to date (${version})"
    exit 0
fi

if [[ -n "$check_only" ]]; then
    echo "update available: ${current} -> ${latest} (${version})"
    exit 10
fi

info "Updating ${current} -> ${latest}"

# Reuse install.sh rather than duplicating download/verify/atomic-replace.
# --no-service keeps this to a binary swap: the unit file and config are
# already in place, and the restart below is the part we want to control.
installer="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2> /dev/null && pwd)/install.sh"
if [[ -r "$installer" ]]; then
    bash "$installer" --version "$version" --target "$target" --bin-dir "$bin_dir" --no-service
else
    fetch_stdout "$RAW_INSTALLER" \
        | bash -s -- --version "$version" --target "$target" --bin-dir "$bin_dir" --no-service
fi

SUDO=""
if [[ "$(id -u)" -ne 0 ]]; then
    command -v sudo > /dev/null || die "must run as root to restart ${unit}"
    SUDO="sudo"
fi

if [[ -n "$no_restart" ]]; then
    info "Skipping restart (--no-restart); run: sudo systemctl restart ${unit}"
elif ! command -v systemctl > /dev/null || [[ ! -d /run/systemd/system ]]; then
    echo "warning: systemd not detected; the new binary is installed but nothing was restarted" >&2
elif ! $SUDO systemctl cat "$unit" > /dev/null 2>&1; then
    echo "warning: ${unit} is not installed; skipped the restart" >&2
else
    info "Restarting ${unit}"
    $SUDO systemctl restart "$unit"
    $SUDO systemctl is-active --quiet "$unit" \
        || die "${unit} did not come back up; check: journalctl -u ${unit} -n 50"
fi

installed_version="$("${bin_dir}/${BIN_NAME}" -V 2> /dev/null || "$installed" -V)"
info "Now running ${installed_version}"

# If the release's binary reports a different version than its tag, every
# future run would see the same "update available" and restart the service
# again. Say so loudly rather than quietly looping on a cron schedule.
running="$(printf '%s' "$installed_version" | awk '{print $2}')"
if [[ "$running" != "$latest" ]]; then
    echo "warning: ${version} contains a binary reporting ${running}, not ${latest}." >&2
    echo "warning: re-running this updater would keep re-installing it; pin --version ${version} or fix the release." >&2
    exit 1
fi
