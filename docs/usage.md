# Using redir-rust

[Back to README](../README.md)

## Usage

Run `redir-rust` without arguments to open a terminal menu with Start, Stop,
Setup Config, Edit Config, Status, Monitor, Open GUI, Check for updates, and Exit. Run `redir-rust --gui`
to open the graphical manager directly in your browser. The GUI listens on all
IPv4 interfaces by default, on an automatic port. It prints a per-run access
URL with a detected network IP when available. Use `--gui-bind IP:PORT` to select a
specific interface and port. Keep the access URL private because it grants
config and service control. The GUI uses HTTP, so use it on a trusted LAN or
behind HTTPS. The GUI closes
when the command exits. It can add and remove redirects,
edit validated TOML, and control the service. Setup Config guides you through a
named TCP or UDP redirect: public listen address and port, one or more backend
addresses and ports in failover order, timeout, and an optional offline MOTD
plugin checkbox. Setup checks the chosen listen address with a temporary TCP or
UDP bind and asks for another address if it is occupied or cannot be bound.
The check releases the socket immediately; startup checks availability again.
Review the settings before saving. On Linux Start and Stop
control the systemd service. On Windows they control a background process using
the config selected by `settings.json`; its PID stays in `%APPDATA%\redir-rust`
and its log is stored beside the selected config.

The main menu, **Setup Config**, and **Edit Config** use Clack-style prompts powered by
`cliclack`: arrow keys select options, Space toggles plugin checkboxes, and Enter
confirms. Each interactive screen clears the terminal before displaying its
current prompt; save screens keep the review visible above the choices.
Address menus offer all IPv4 interfaces (listen), localhost (backend),
Tailscale, or manual IPv4/IPv6 entry. Editing also offers the current address.
Tailscale fetches a searchable device picker from `tailscale status --json`:
listen prompts offer this machine's addresses; backend prompts offer peer devices
with names, IPv4/IPv6 addresses, and online status. Choose an address, then enter
the port. Tailscale must be installed, running, and signed in. If fetching fails,
you can retry or enter an IP manually; Previous returns to the address menu.
Piped input keeps the plain text prompts, including the `tailscale` shortcut.

📝 **Edit Config** lists the service names from the selected config file,
then lets you edit the listen IP/port, ordered destinations, service name,
TCP/UDP protocol, or timeout. Choose **⬅ Previous** (or press Esc) to go back;
value menus offer Keep, Enter a new value, and Previous. Save review offers
Save changes, Discard changes, and Previous. Esc cancels pending input; piped
value prompts also accept `back`. Menus support
arrow keys on a terminal and numbered choices when piped. Each change has a
review/save prompt and is validated before writing; other service blocks are
preserved. **🛠 Advanced editor** opens the selected named service in `$EDITOR`
for plugin text and other options (the full file for an unnamed service).
Restart redir-rust after saving to apply the changes. `-e` / `--edit-config`
still opens the full config in your editor directly.

Run `redir-rust --monitor` to view live connections and traffic in a terminal.
The dashboard reads the running service's snapshots in `/run/redir-rust/` once
per second. Press Tab to filter by redirect, Up/Down to select a connection,
`s` to sort, or `q` to quit.

Run a single redirect from CLI flags:

```sh
redir-rust --listen 0.0.0.0:25565 --target 127.0.0.1:25566
```

Repeat `--target` to provide failover targets in priority order:

```sh
redir-rust --listen 0.0.0.0:25565 \
  --target 10.0.0.10:25566 \
  --target 10.0.0.11:25566
```

Or run one or more redirects defined in a TOML file (see `config.example.toml`):

```sh
redir-rust --config config.toml
```

In run mode, `--config` selects the file and rejects redirect flags such as
`--listen` and `--target` so no setting is silently ignored.

Manage named redirects in a config file:

```sh
redir-rust --add --name survival --listen 0.0.0.0:25565 --target 127.0.0.1:25566
redir-rust --edit survival
redir-rust --remove survival
```

`--add` accepts the same redirect flags as direct run mode, including repeated
`--target`. `--edit NAME` opens only that redirect in `$EDITOR` and validates
the complete file before saving. Duplicate names are rejected. These commands
do not restart a running instance. The config location is set in
`/etc/local/redir-rust/settings.json` on Linux or
`%APPDATA%\redir-rust\settings.json` on Windows:

```json
{"config_path": "config.toml"}
```

Relative paths are resolved beside `settings.json`; absolute paths also work.
If the settings file is missing, the config defaults to `config.toml` in that
directory. The Linux installer creates `settings.json` and the config file;
on Windows, copy `settings.example.json` to the path above to change the
location. `--settings FILE` chooses another settings file, and `--config FILE`
overrides its `config_path`. If you used the older Linux path, move your config
to the new path before starting the service.

Pass `--debug` for verbose logging (shorthand for `RUST_LOG=debug`; an
explicit `RUST_LOG` env var still takes precedence). See all available flags:

```sh
redir-rust --help
```

## Targets and failover

Each `[[redirect]]` must set exactly one of `target` or `targets`. The legacy
singular form remains supported:

```toml
target = "127.0.0.1:25566"
```

For failover, use a non-empty array ordered from highest to lowest priority:

```toml
targets = [
  "10.0.0.10:25566", # primary
  "10.0.0.11:25566", # secondary
]
```

The two forms are mutually exclusive, and `targets = []` is invalid. On the
CLI, repeated `--target` flags build the same ordered list.

For TCP, every new connection tries the targets sequentially until one
connects. `connect_timeout_ms` (or `--connect-timeout`) applies separately to
each target attempt. Once connected, that client stays pinned to the selected
backend for the life of the connection. New connections always begin with the
primary again, so they automatically fail back after it recovers. Failure
plugins run only after every target attempt fails.

Multi-target UDP failover is specifically for Bedrock servers: on the first
datagram of each new client session, the relay asynchronously probes the
ordered targets with RakNet `Unconnected Ping` packets. Initial client
datagrams are queued during selection, then flushed to the first responding
backend; that session remains pinned there until its UDP idle timeout expires.
New sessions start at the primary again. If all probes fail, the Bedrock
offline plugin handles the queued packets when enabled. Single-target generic
UDP does not receive Bedrock preflight probes; it remains a direct relay.
