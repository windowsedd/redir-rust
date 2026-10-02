# Package publishing

[Back to README](../README.md)

## Release workflow

Run `bash scripts/release.sh <VERSION> --push` from a clean checkout to create
and push a new version tag. The Release workflow tests and builds x86_64 Linux
(GNU and static musl) and Windows (MSVC), publishes archives and checksums,
and creates `.deb` and `.rpm` packages from the static musl binary.

For a failed build or upload, dispatch **Release** with the existing tag.
The tag must match `Cargo.toml` and `Cargo.lock`. Linux package uploads use the workflow's built-in `GITHUB_TOKEN`. WinGet and
Chocolatey submissions use their own optional secrets, described below.

## Linux

[nFPM](https://nfpm.goreleaser.com/docs/configuration/) builds packages from
[linux/nfpm.yaml](linux/nfpm.yaml). The workflow pins the tool version and
verifies its download checksum.

Packages contain:

- `/usr/bin/redir-rust` and `/usr/lib/systemd/system/redir-rust.service`.
- An inactive `/etc/local/redir-rust/config.toml` and `settings.json`, marked
  as configuration files to preserve local edits during upgrades.
- An example config and the MIT license.

Install with `sudo apt install ./<file>.deb` or `sudo dnf install ./<file>.rpm`.
Configure a redirect before enabling the service. Installation reloads systemd
but does not start or restart it; after an upgrade, run
`sudo systemctl restart redir-rust` when you can drop active connections.
Removal stops and disables the service. Upgrades leave it running until the
explicit restart.

Use package-manager upgrades for these installations. The shell installer uses
`/usr/local/bin` and `/etc/systemd/system`; migrate those older installations
before switching to packages so an older binary or unit does not take precedence.
No APT/DNF repository is configured: download packages from GitHub releases.

To package locally, install nFPM 2.47.0 and run from the repository root:

```sh
cargo build --release --locked --target x86_64-unknown-linux-musl
mkdir -p dist
sed 's|/usr/local/bin/redir-rust|/usr/bin/redir-rust|' packaging/systemd/redir-rust.service > dist/redir-rust.service
export REDIR_VERSION=0.2.0
nfpm package --config packaging/linux/nfpm.yaml --packager deb --target dist/redir-rust.deb
nfpm package --config packaging/linux/nfpm.yaml --packager rpm --target dist/redir-rust.rpm
```

## WinGet

Package identifier: **`windowsedd.redir-rust`**. The manifests install the release
ZIP as a portable package and expose the `redir-rust` command. The generator
hashes the actual ZIP and checks its nested executable path. The installer declares
the Visual C++ x64 runtime dependency used by the MSVC binary.

[Publish to WinGet](../.github/workflows/winget.yml) supports both the first
submission and later versions through Microsoft's
[WinGet Manifest Creator](https://github.com/microsoft/winget-create).
The Release workflow calls it after uploading assets; a separate release-event
trigger would not fire for releases created with the built-in `GITHUB_TOKEN`.

To activate submission:

1. Create a classic GitHub personal access token with `public_repo`, following
   [Microsoft's token instructions](https://github.com/microsoft/winget-create/blob/main/doc/token.md).
2. Add it to this repository's Actions secrets as **`WINGET_TOKEN`**. The workflow
   passes it through `WINGET_CREATE_GITHUB_TOKEN`, keeping it out of CLI arguments.
3. Dispatch **Publish to WinGet** with an existing stable release tag such as
   `v0.2.0`. The workflow downloads the published ZIP, generates manifests,
   uploads them as an Actions artifact, and opens a PR in `microsoft/winget-pkgs`.
4. Follow the PR's validation and review. The package becomes available through
   WinGet after Microsoft merges and indexes it.

Without the secret, the workflow generates an artifact and reports that
submission was skipped. It does not claim the package is listed.
Prerelease tags are excluded. Repeated submissions of the same version can
create duplicate PRs; inspect existing PRs before manually rerunning submission.

The verified [v0.2.0 manifests](winget/0.2.0) are also included for the initial
submission. To generate and submit manually from Windows:

```powershell
python scripts/winget-manifest.py v0.2.0 redir-rust-v0.2.0-x86_64-pc-windows-msvc.zip
winget validate --manifest dist/winget/0.2.0
wingetcreate submit dist/winget/0.2.0 --no-open
```

WingetCreate prompts for GitHub authentication if you have no cached token.
Use [Microsoft's submission guidance](https://learn.microsoft.com/windows/package-manager/package/repository)
for validation failures or community review requirements.


## Chocolatey

Package identifier: **`redir-rust`**. The package embeds the official Windows x64
executable, its MIT license, and a `VERIFICATION.txt` with the source URL and
ZIP/executable SHA-256 hashes. Chocolatey creates the command shim on install
and removes it on uninstall. The `vcredist140` dependency supplies the MSVC
runtime. Installation leaves application configuration and background processes
alone; configuration lives under `%APPDATA%\redir-rust`.

[Publish to Chocolatey](../.github/workflows/chocolatey.yml) runs after a stable
GitHub release, or can be dispatched with an existing tag such as `v0.2.0`.
It prepares the package, runs `choco pack`, tests installation and the command
shim, verifies the installed version, and tests uninstall before uploading.
The smoke test uses the Windows runner's existing Visual C++ runtime and skips
dependency installation; normal users receive the declared dependency.
The workflow also saves the `.nupkg` as an Actions artifact.

To enable uploads:

1. Sign in to [Chocolatey's account page](https://push.chocolatey.org/account)
   and copy your API key, following the
   [official API key instructions](https://docs.chocolatey.org/en-us/create/commands/api-key/).
2. Add it to this repository's Actions secrets as **`CHOCOLATEY_API_KEY`**.
3. Dispatch **Publish to Chocolatey** for an existing stable release, or push
   your next release tag. The workflow uploads to `https://push.chocolatey.org/`.
4. Follow Chocolatey's moderation feedback before announcing availability.

Without the secret, the workflow builds and tests the package, saves the
artifact, and reports that publishing was skipped. Check the community feed
before resubmitting an existing version: published versions cannot be overwritten.

To build manually on Windows from a downloaded release ZIP:

```powershell
python scripts/chocolatey-package.py v0.2.0 redir-rust-v0.2.0-x86_64-pc-windows-msvc.zip
choco pack dist/chocolatey/0.2.0/redir-rust.nuspec --output-directory dist
choco install redir-rust --version 0.2.0 --source "$pwd/dist" -y
```

Use [Chocolatey's package creation guidance](https://docs.chocolatey.org/en-us/create/create-packages/)
for manual testing and publishing. After approval, users can run
`choco install redir-rust -y` and `choco upgrade redir-rust -y`.
