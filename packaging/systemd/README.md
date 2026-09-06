# systemd service

Installs `redir-rust` as a systemd service that reads its redirects from
`/etc/redir-rust/config.toml`.

## Quick setup

The installer is built into the binary itself (`--install-systemd`), so
once you have a built `redir-rust`, no repo checkout or separate script is
needed on the target host — a bare `scp`'d binary is enough:

```sh
cargo build --release
sudo ./target/release/redir-rust --install-systemd
```

Copies itself to `/usr/local/bin/redir-rust`, drops a default `config.toml`
into `/etc/redir-rust/` (only if one doesn't already exist), installs the
unit file, and enables + (re)starts the service. Safe to re-run after
rebuilding to upgrade — it won't touch an existing config, and it always
restarts the service so the new binary actually takes effect (the copy
itself uses a temp file + atomic rename, so it works even while the old
binary is running as the current service — a plain overwrite would fail
with `Text file busy`). Requires root.

Always run it via an explicit path to the binary you just built (e.g.
`sudo ./target/release/redir-rust --install-systemd`), not a bare
`redir-rust --install-systemd` — on a fresh host nothing is on `$PATH` yet
for bash to find.

Edit the config with `sudo redir-rust -e` (opens `$EDITOR`, or falls back to
`nano`/`vi`, and validates the file when you save — see below), then:
`sudo redir-rust --restart`.

If you don't want to build at all, [`install.sh`](../../install.sh) in the
repo root downloads a published release binary instead and then calls this
same `--install-systemd`:

```sh
curl -fsSL https://raw.githubusercontent.com/windowsedd/redir-rust/main/install.sh | sudo bash
```

[`update.sh`](../../update.sh) is the upgrade counterpart: it only acts when
a newer release exists, then restarts the unit (`--unit`, default
`redir-rust.service`).

`packaging/systemd/install.sh` does the same thing as a standalone shell
script, if you'd rather not run the binary as an installer (e.g. you want
to review the exact commands before running as root):

```sh
./packaging/systemd/install.sh
```

## Manual setup

Equivalent to what `--install-systemd` does, if you'd rather run the steps yourself:

```sh
cargo build --release
sudo install -Dm755 target/release/redir-rust /usr/local/bin/redir-rust
sudo mkdir -p /etc/redir-rust
sudo cp config.example.toml /etc/redir-rust/config.toml   # edit to taste
sudo install -Dm644 packaging/systemd/redir-rust.service /etc/systemd/system/redir-rust.service

sudo systemctl daemon-reload
sudo systemctl enable --now redir-rust
```

The unit is otherwise plain — `Restart=on-failure`, no `User=` (runs as
root), no sandboxing directives (`DynamicUser`, `ProtectSystem`,
capabilities, etc). That's a deliberate tradeoff: this redirector typically
listens on a non-privileged port, and sandboxing directives are a common
source of "works when run by hand, breaks under systemd" failures that are
hard to diagnose from `systemctl status` alone. If you need to bind a
privileged port (<1024) without running as root, add
`AmbientCapabilities=CAP_NET_BIND_SERVICE` back yourself and test carefully.

It uses `Type=notify`: the binary sends systemd a `READY=1` notification
once its listeners are up, and a live `STATUS=` update (via
`$NOTIFY_SOCKET`, no `libsystemd` dependency) every time a connection opens
or closes. It also rewrites its own process title on every change (a
direct, dependency-free argv-memory overwrite bounded by `/proc/self/stat`'s
`arg_start`/`arg_end`, *not* the common `prctl(PR_SET_NAME)` trick, which
only changes `/proc/pid/comm` and wouldn't show up here). Together, that
means `systemctl status redir-rust` shows who's connected *right now* in
both the `Status:` line and the `CGroup:` process listing, without running
`--service-status` at all:

```text
● redir-rust.service - redir-rust port redirector
     Loaded: loaded (/etc/systemd/system/redir-rust.service; enabled; vendor preset: enabled)
     Active: active (running) since Wed 2026-07-01 13:13:00 UTC; 31min ago
     Status: "2 connection(s): [1.2.3.4:52344] [5.6.7.8:9001]"
   Main PID: 350067 (redir-rust)
      Tasks: 3
     CGroup: /system.slice/redir-rust.service
             └─350067 redir-rust [2 connection(s): [1.2.3.4:52344] [5.6.7.8:9001]]
```

Only the first 8 client addresses are listed (plus a `+N more` suffix) so
the line stays a readable one-liner under heavy load; the process title is
additionally capped to whatever byte length the original
`/usr/local/bin/redir-rust --config ...` command line happened to occupy
(that's a kernel-enforced ceiling on argv-rewrite, not something we can
raise), so it may truncate sooner than the `Status:` line does. The full
list with per-connection duration/target is still in `--service-status`'s
`active_connections`.

## Controlling the service

```sh
sudo redir-rust --start           # systemctl start redir-rust.service
sudo redir-rust --stop            # systemctl stop redir-rust.service
sudo redir-rust --restart         # systemctl restart redir-rust.service, e.g. after editing config.toml
sudo redir-rust --service-status  # full JSON status, see below
journalctl -u redir-rust -f       # tail logs
```

`--start`/`--stop`/`--restart`/`--service-status` all target
`redir-rust.service` by default; override with `--unit <name>` if you
installed it under a different unit name. These are thin `systemctl`
wrappers (same root requirement as `systemctl` itself) kept on the binary
so you don't have to remember the unit name for routine operations.

## Editing the config

```sh
sudo redir-rust -e                       # edits /etc/redir-rust/config.toml
sudo redir-rust -e --config ./other.toml # edits a different file
```

Creates the file from the built-in default template first if it doesn't
exist yet, opens it in `$EDITOR` (falling back to `$VISUAL`, then
`nano`/`vi` on Linux or `notepad` on Windows), and re-parses it after the
editor exits — printing `config OK (N redirect(s))` or a `warning: config
has an error: ...` so a typo doesn't go unnoticed until the next restart.
Does **not** restart the service itself; follow up with `sudo redir-rust
--restart` once you're happy with the changes.

## Machine-readable status

`redir-rust --service-status` (built into the binary) wraps `systemctl
status` and prints JSON (unit, status, loaded, active, since, main_pid,
tasks, memory, cpu, cgroup, processes) — handy for a bot/dashboard that
wants to render a status card instead of parsing `systemctl status` text
itself. It also loads `[[redirect]]` entries from `--config` (default
`/etc/redir-rust/config.toml`, using the same TOML parser the service
itself uses) and probes each TCP target with a 2s connect attempt, so you
can see per-redirect backend health alongside the service state. UDP
redirects are listed but not probed (`reachable: null`). It pulls the last
10 connection-failure-looking lines (`failed`, `refused`, `error`, `timed
out`, `unreachable`) from `journalctl -u <unit>` into `recent_errors`. And
it reports **who's connected right now**: the running service maintains a
live registry of open connections (updated on every connect/disconnect)
and snapshots it to `/run/redir-rust/connections.json`; `--service-status`
reads that file directly into `active_connections`.

```sh
redir-rust --service-status --unit redir-rust.service --config /etc/redir-rust/config.toml
```

`packaging/systemd/service-status.sh <unit-name> [config-file]` is the
original standalone-script equivalent, kept for cases where you'd rather
not invoke the binary itself (e.g. from a monitoring box that doesn't have
`redir-rust` installed). It reads the same `/run/redir-rust/connections.json`
file for `active_connections`.

```json
{
  "unit": "redir-rust.service",
  "status": "active",
  "active": "active (running)",
  "since": "Wed 2026-07-01 13:13:00 UTC; 31min ago",
  "main_pid": "2996715 (redir-rust)",
  "tasks": "2 (limit: 23851)",
  "memory": "1.2M",
  "cpu": "122ms",
  "cgroup": "/system.slice/redir-rust.service",
  "processes": [{ "pid": 2996715, "command": "/usr/local/bin/redir-rust --config /etc/redir-rust/config.toml" }],
  "targets": [
    { "name": "minecraft-survival", "listen": "0.0.0.0:25565", "target": "127.0.0.1:25566", "protocol": "tcp", "reachable": true },
    { "name": "bedrock-relay", "listen": "0.0.0.0:19132", "target": "127.0.0.1:19133", "protocol": "udp", "reachable": null }
  ],
  "recent_errors": [
    "2026-07-01T05:12:52+0000 host redir[2995858]: Failed connecting to target 100.85.222.77: Connection refused"
  ],
  "active_connections": [
    { "id": 7, "redirect": "minecraft-survival", "protocol": "tcp", "client": "1.2.3.4:52344", "target": "127.0.0.1:25566", "connected_at_unix": 1751349999, "duration_secs": 42 }
  ]
}
```
