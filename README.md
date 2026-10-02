# redir-rust

**Forward TCP and UDP ports. Keep a fallback ready.**

A Rust port redirector for Linux and Windows, inspired by `redir`. Route traffic
to one backend or an ordered list of targets. Manage redirects through the
terminal menu, a browser GUI, or TOML. Minecraft Java and Bedrock plugins show
an offline message when the backend is unreachable.

[![CI](https://github.com/windowsedd/redir-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/windowsedd/redir-rust/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/windowsedd/redir-rust)](https://github.com/windowsedd/redir-rust/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[Install](#install) · [Quick start](#quick-start) · [Configuration](#configuration) ·
[Management](docs/usage.md) · [Plugins](docs/plugins.md) · [Publishing](packaging/README.md)

| Capability | Behavior |
| --- | --- |
| TCP failover | Try backends in order for each new connection; return to the primary when it recovers. |
| UDP relay | Forward generic UDP to one target; use RakNet probes for multi-target Bedrock failover. |
| Offline responses | Show a Minecraft server-list MOTD and disconnect message when all backends are down. |
| Management | Guided setup and editing, browser GUI, service controls, and live terminal monitoring. |
| TCP traffic shaping | Set a bandwidth cap, random delay, and transfer buffer size. |

## Install

### Linux

Download and install the latest static x86_64 binary, with checksum verification
and systemd setup:

```sh
curl -fsSL https://raw.githubusercontent.com/windowsedd/redir-rust/main/install.sh | sudo bash
```

A fresh install creates an inactive config and leaves the service stopped.
Run `sudo redir-rust`, choose **Setup Config**, and add a redirect. Then enable
it at boot:

```sh
sudo systemctl enable --now redir-rust
```

The installer preserves existing configuration. Use `--no-service` for a binary
only, or `--help` for version, target, and destination options:

```sh
curl -fsSL https://raw.githubusercontent.com/windowsedd/redir-rust/main/install.sh | sudo bash -s -- --help
```

The release workflow also builds `.deb` and `.rpm` packages for x86_64 Linux.
Once published, download the matching file from [Releases](https://github.com/windowsedd/redir-rust/releases)
and install it:

```sh
sudo apt install ./redir-rust-v<VERSION>-amd64.deb
# Fedora / RHEL:
sudo dnf install ./redir-rust-v<VERSION>-x86_64.rpm
```

Packages install to `/usr/bin`, preserve config on upgrades, and include a
systemd unit. Configure before starting; restart the service after upgrading.
Use your original installation method for updates to keep binary paths consistent.

### Windows

Download the Windows x64 ZIP from [Releases](https://github.com/windowsedd/redir-rust/releases/latest),
extract it, and run `redir-rust.exe`. Add its directory to `PATH` to use it
from any terminal.

WinGet publishing is configured for package ID `windowsedd.redir-rust`.
**After Microsoft accepts the initial package submission**, install with:

```powershell
winget install --exact --id windowsedd.redir-rust
```

See [publishing setup](packaging/README.md#winget) for the submission token,
workflow, and initial package steps.

Chocolatey publishing is configured for package ID `redir-rust`. After the
package passes community moderation:

```powershell
choco install redir-rust -y
```

See [Chocolatey publishing setup](packaging/README.md#chocolatey) to enable uploads.

## Quick start

Forward a local port:

```sh
redir-rust --listen 0.0.0.0:25565 --target 127.0.0.1:25566
```

Add a fallback backend:

```sh
redir-rust --listen 0.0.0.0:25565 \
  --target 10.0.0.10:25566 \
  --target 10.0.0.11:25566
```

Use `--protocol udp` for a UDP relay. Multi-target UDP failover is specific to
Minecraft Bedrock; single-target UDP supports generic traffic.

For guided setup, run without arguments:

```sh
redir-rust
```

Choose **Setup Config** to add a named redirect, then **Start** to run it.
Linux service controls require root. On Windows, Start runs a background
process and stores its PID under `%APPDATA%\redir-rust`.

## Configuration

Run several redirects from one TOML file:

```toml
[[redirect]]
name = "survival"
listen = "0.0.0.0:25565"
targets = ["10.0.0.10:25566", "10.0.0.11:25566"]
protocol = "tcp"
connect_timeout_ms = 5000

[redirect.minecraft.plugins]
enabled = true
motd_line1 = "Server offline"
motd_line2 = "Please try again later"
```

```sh
redir-rust --config config.toml
```

Use exactly one of `target` or a non-empty `targets` array. Each TCP connection
stays on its selected backend; new connections start at the primary again.
The connection timeout applies to each backend attempt.

| File | Linux | Windows |
| --- | --- | --- |
| Settings | `/etc/local/redir-rust/settings.json` | `%APPDATA%\redir-rust\settings.json` |
| Default config | `/etc/local/redir-rust/config.toml` | `%APPDATA%\redir-rust\config.toml` |

Settings use `{"config_path": "config.toml"}`. Relative paths resolve beside
the settings file. `--settings FILE` selects another settings file;
`--config FILE` overrides its config path. In run mode, `--config` rejects
redirect flags so settings cannot silently conflict.

[Full config example](config.example.toml) · [Settings example](settings.example.json) ·
[Target selection and editing](docs/usage.md#targets-and-failover)

## Manage and monitor

| Command | Purpose |
| --- | --- |
| `redir-rust` | Open the menu: Start, Stop, Setup Config, Edit Config, Status, Monitor, Open GUI, Reload, Check for updates, Exit. |
| `redir-rust --gui` | Open the browser manager. |
| `redir-rust --monitor` | View service connection and traffic snapshots from `/run/redir-rust/` on Linux. |
| `redir-rust --reload` | Apply the saved config while established clients keep their sessions. |
| `redir-rust --service-status` | Show service status. |
| `redir-rust --add --name NAME --listen IP:PORT --target IP:PORT` | Add a named redirect. |
| `redir-rust --edit NAME` | Edit one redirect in `$EDITOR`. |
| `redir-rust --remove NAME` | Remove a redirect. |
| `redir-rust --edit-config` | Edit the full configuration. |
| `redir-rust --help` | List flags; use `--debug` for verbose logging. |

The GUI binds all IPv4 interfaces on an automatic port by default. Its per-run
URL grants configuration and service control: keep it private and use a trusted
LAN or HTTPS proxy. Use `--gui-bind 127.0.0.1:8080` for local access.
After saving, choose **Reload** in the menu or GUI. Reload applies changes to new
connections while established TCP connections and UDP sessions keep their
original backend and settings. Invalid configuration or a failed new bind leaves
the running configuration intact. Restart remains available for binary upgrades.

```sh
sudo redir-rust --reload
# For a manually started instance:
redir-rust --reload --config config.toml
# With the updated systemd unit:
sudo systemctl reload redir-rust
```

Reload requires an instance started with `--config` or `--settings`. Removing a
TCP listener closes its listening socket while established connections continue.
Removed UDP listeners accept packets only from existing clients until their
original idle timeouts expire. A listen-address change that overlaps an existing
or draining socket is rejected; move to a free address or drain the old listener
before reusing its port. See [reload behavior](docs/usage.md#graceful-reload).

[Guided editing, Tailscale address picker, and monitoring keys](docs/usage.md) ·
[systemd details](packaging/systemd/README.md)

## Minecraft offline responses

Enable the Java plugin for TCP, or the Bedrock plugin for UDP. After the
backends fail, clients receive a configured offline MOTD on server-list pings
and an offline disconnect message on join attempts.

![Java server list showing the offline MOTD](assets/Java_serverlist.png)

[Plugin configuration, screenshots, protocol coverage, and Bedrock limitations](docs/plugins.md) ·
[TCP traffic shaping](docs/plugins.md#traffic-shaping-tcp-only)

## Update

Choose **Check for updates** in the menu to compare your version with the latest
GitHub release. Checking requires `curl` and leaves the service running.

For Linux installations made with `install.sh`:

```sh
curl -fsSL https://raw.githubusercontent.com/windowsedd/redir-rust/main/update.sh | sudo bash
```

The updater verifies the checksum and restarts the service when it installs a
new version, dropping open connections. Use `--no-restart` to restart later.
`--check` reports availability without installing (exit `10` for an update,
`0` when current); `--force` reinstalls the same version.

For `.deb` or `.rpm` installations, install the newer release package and run
`sudo systemctl restart redir-rust`. Once available through WinGet, Windows
installations can use `winget upgrade --exact --id windowsedd.redir-rust`.
For Chocolatey installations, use `choco upgrade redir-rust -y`.

## Build and release

```sh
cargo build --release
cargo test --locked
```

Build on the target OS. For a static Linux binary from Windows, use
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) with Zig:

```sh
scoop install zig
cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

`redir-rust --version` reports the version, git commit, target, build profile,
and compiler. A dirty checkout adds `-dirty` to the commit.

Cut a new release from a clean checkout:

```sh
bash scripts/release.sh <VERSION> --push
```

The script synchronizes `Cargo.toml`, `Cargo.lock`, and the tag, runs tests,
then commits and pushes. The [release workflow](.github/workflows/release.yml)
builds GNU/musl Linux archives, Windows ZIPs, Linux packages, and SHA-256 sums,
then generates WinGet manifests and Chocolatey packages. Submission uses the
configured WinGet token or Chocolatey API key.

[Publishing setup and recovery](packaging/README.md)

## License

[MIT](LICENSE)
