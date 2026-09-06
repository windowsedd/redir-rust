#!/usr/bin/env bash
# Cut a redir-rust release: bump Cargo.toml, refresh Cargo.lock, commit, tag.
#
#   ./scripts/release.sh 0.2.0        # or v0.2.0
#   ./scripts/release.sh 0.2.0 --push
#
# The tag, Cargo.toml and Cargo.lock must agree -- the release workflow's
# verify-version job refuses a tag that doesn't match, and a mismatched
# version would make update.sh see a permanent "update available" and
# restart the service on every run. Doing all three here is what keeps them
# from drifting.
#
# Options:
#   --push      push the branch and tag when done (otherwise prints the commands)
#   --no-verify skip the pre-tag cargo test run
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

push=""
verify=1
new_version=""

die() {
    echo "error: $*" >&2
    exit 1
}

info() { echo "==> $*"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --push) push=1; shift ;;
        --no-verify) verify=""; shift ;;
        -h | --help)
            awk 'NR > 1 { if ($0 ~ /^#/) { sub(/^# ?/, ""); print } else exit }' "$0"
            exit 0
            ;;
        -*) die "unknown option: $1 (try --help)" ;;
        *)
            [[ -z "$new_version" ]] || die "give exactly one version"
            new_version="$1"
            shift
            ;;
    esac
done

[[ -n "$new_version" ]] || die "usage: scripts/release.sh <version> [--push] (try --help)"

# Accept either 0.2.0 or v0.2.0; Cargo wants the bare form, git the v-prefix.
new_version="${new_version#v}"
[[ "$new_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
    || die "not a semver version: ${new_version}"
tag="v${new_version}"

command -v cargo > /dev/null || die "cargo not found"
[[ -z "$(git status --porcelain --untracked-files=no)" ]] \
    || die "working tree has uncommitted changes; commit or stash them first"
git rev-parse -q --verify "refs/tags/${tag}" > /dev/null && die "tag ${tag} already exists"

current="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
[[ "$current" != "$new_version" ]] || die "Cargo.toml is already at ${new_version}"
info "Bumping ${current} -> ${new_version}"

# Only the [package] version: dependency versions are inline table values
# (`tokio = { version = ... }`) and never start a line.
tmp="$(mktemp)"
awk -v v="$new_version" '
    !done && /^version[[:space:]]*=/ { print "version = \"" v "\""; done = 1; next }
    { print }
' Cargo.toml > "$tmp"
mv "$tmp" Cargo.toml

info "Refreshing Cargo.lock"
cargo update --offline -p redir-rust > /dev/null 2>&1 || cargo update -p redir-rust > /dev/null

locked="$(grep -A2 '^name = "redir-rust"$' Cargo.lock | grep -m1 '^version' | cut -d'"' -f2)"
[[ "$locked" == "$new_version" ]] || die "Cargo.lock still records ${locked}; fix it before tagging"

if [[ -n "$verify" ]]; then
    info "Running cargo test --locked"
    cargo test --locked --quiet
fi

info "Committing and tagging ${tag}"
git add Cargo.toml Cargo.lock
git commit -q -m "chore: release ${tag}"
git tag -a "$tag" -m "redir-rust ${tag}"

branch="$(git rev-parse --abbrev-ref HEAD)"
if [[ -n "$push" ]]; then
    info "Pushing ${branch} and ${tag}"
    git push origin "$branch"
    git push origin "$tag"
    info "Release workflow will build and publish ${tag}"
else
    cat <<EOF

Tagged ${tag} locally. To publish (this triggers the release workflow):
  git push origin ${branch}
  git push origin ${tag}

To undo before pushing:
  git tag -d ${tag} && git reset --hard HEAD~1
EOF
fi
