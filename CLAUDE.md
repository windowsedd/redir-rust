# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

- Build (debug): `cargo build`
- Build (release, LTO'd): `cargo build --release`
- Run: `cargo run -- --listen 0.0.0.0:25565 --target 127.0.0.1:25566`
- Run with failover targets: `cargo run -- --listen 0.0.0.0:25565 --target 10.0.0.10:25566 --target 10.0.0.11:25566`
- Run with a config file: `cargo run -- --config config.example.toml`
- Check without building: `cargo check`
- Lint: `cargo clippy`
- Format: `cargo fmt`
- Tests: `cargo test` (unit tests live alongside the source; integration tests are under `tests/`)
- Verbose logging: set `RUST_LOG` (e.g. `RUST_LOG=debug cargo run -- ...`); defaults to `info` via `tracing-subscriber`'s `EnvFilter`.

## Architecture

`redir-rust` is a Tokio-based TCP/UDP port redirector (like `redir`) with a plugin hook system. Entry point is `src/main.rs`.

- **Config resolution** (`src/config.rs`): a run is driven by either a `--config <file.toml>` (one or more `[[redirect]]` entries, deserialized via `FileConfig`/`RedirectConfig`) or a single redirect built directly from CLI flags. Every redirect has a listener and a non-empty ordered target list. TOML accepts exactly one of the backward-compatible singular `target` or the `targets` array; CLI `--target` is repeatable and preserves priority order. Both paths converge on the same `RedirectConfig` struct before dispatch, so `main.rs` only ever deals with a `Vec<RedirectConfig>`.
- **Dispatch** (`src/main.rs`): each `RedirectConfig` is spawned as its own `tokio::task` via a `JoinSet`, one task per redirect, running independently until the process exits. Protocol (`Protocol::Tcp`/`Udp`) selects between `run_tcp_redirect` and `run_udp_redirect`.
- **TCP path** (`src/proxy.rs`): `run()` binds a listener and, per accepted connection, calls plugin `on_connect` hooks, then tries targets sequentially in priority order. `connect_timeout_ms` applies independently to every target attempt. On success the connection is pinned to that backend and pipes bytes bidirectionally through `conn_worker`; every new connection starts from the primary, providing automatic failback. Only after all targets fail do `on_target_failure` hooks get a chance to take over the raw client socket (see Minecraft plugin below) before it is dropped.
- **UDP path** (`src/udp_proxy.rs`): connectionless relay. A single-target redirect keeps generic UDP behavior: the first datagram creates a direct outbound `UdpSocket` session without a Bedrock probe. A multi-target redirect is Bedrock-specific: the first datagram starts asynchronous, sequential RakNet `Unconnected Ping` probes while initial packets are queued; the first responding backend is selected, queued packets are flushed, and that client session stays pinned until `idle_timeout`. New sessions retry the primary. If every target fails, the Bedrock offline plugin handles queued packets when enabled. Each selected session has a dedicated return-path task relaying target→client datagrams. Minecraft flags remain TCP-only and warn on UDP; Bedrock offline settings are UDP-only.
- **Plugin trait** (`src/plugin.rs`): `Plugin` defines optional async hooks (`applies_to`, `on_connect`, `on_data_receive`, `on_target_failure`) with no-op defaults, so a plugin only implements what it needs. `applies_to(listen_addr)` lets a single plugin instance opt in/out per listener (e.g. by port), since all active plugins for a proxy are shared across every connection on that listener.
- **Minecraft offline MOTD plugin** (`src/plugins/minecraft_offline.rs`): the built-in TCP plugin. It only activates via `on_target_failure` — after every configured backend is unreachable, it speaks just enough of the Minecraft protocol (raw VarInt-framed packet read/write, no external MC library) to handle both cases distinguished by the Handshake's `next_state`: a status ping (1) gets a canned Server List Ping status JSON (custom status line, two MOTD lines, optional base64 favicon); an actual join attempt (2, login) gets kicked with a Login Disconnect carrying the same MOTD text as a VarInt-length-prefixed JSON component. NBT components used by disconnect packets in later protocol states do not apply to Login Disconnect. Any other `next_state` is left unhandled so the proxy just closes the connection.
- **Live connection registry and TCP worker titles** (`src/connections.rs`, `src/conn_worker.rs`, `src/proctitle.rs`): `proxy.rs` and `udp_proxy.rs` register each open TCP connection or UDP session in a process-wide registry using the backend actually selected rather than merely the configured primary. The returned RAII `ConnectionGuard` deregisters it on drop. On every registry change, Unix builds best-effort snapshot the entries to `/run/redir-rust/connections.json` and publish a short connection summary through `sd_notify`'s `STATUS=` field; registry persistence does not change any process title. Separately, each established TCP connection on Unix is handed to a worker child, which sets its own title to `redir-rust <client> -> <actual-selected-target>` (the argv rewrite is effective on Linux). UDP sessions stay in the main process and do not have worker process titles.

### Config file vs CLI flags

`config.example.toml` documents the TOML shape: multiple `[[redirect]]` tables, each independently specifying `listen`, exactly one of `target`/`targets`, `protocol`, and timeouts, with optional protocol-specific plugin sub-tables. When `--config` is passed, all other CLI redirect flags (`--listen`, repeatable `--target`, `--minecraft-offline-motd`, etc.) are ignored — only the config file entries run. The embedded default (used when `--install-systemd`/`-e` need to create a config from scratch) lives at `config::DEFAULT_CONFIG_TOML` (`include_str!` of `config.example.toml`) — single source of truth for both call sites.

### systemd

`packaging/systemd/redir-rust.service` is otherwise plain: `Restart=on-failure`, runs as root, no sandboxing directives (`DynamicUser`/`ProtectSystem`/capabilities). That's deliberate — this redirector's listen ports are typically non-privileged, and the previous hardened unit was a real source of "works by hand, breaks under systemd" failures. It does use `Type=notify` + `NotifyAccess=main` (paired with `src/notify.rs`'s `ready()` call once redirects are spawned in `main.rs`) — get `READY=1` timing wrong here and the unit fails to start, so don't change `Type=` without also checking `notify::ready()` is still called before the process would otherwise block. Several self-contained CLI flags (embed their assets via `include_str!` so they work off a bare deployed binary, no repo checkout needed) manage the rest:

- `--install-systemd` (`src/install.rs`, Unix only): copies itself to `/usr/local/bin`, writes a default config if missing, installs the unit file, `daemon-reload` + `enable --now`. Requires root.
- `--start` / `--stop` / `--restart` (`src/service_ctl.rs`): thin `systemctl <action> <unit>` wrappers; `--unit` (default `redir-rust.service`) is shared with `--service-status`.
- `--service-status` (`src/status.rs`): runs `systemctl status --no-pager -l <unit>` and passes its stdout, stderr, and success/failure status through without parsing or adding JSON fields.
- `-e` / `--edit-config` (`src/edit_config.rs`): opens the config in `$EDITOR`/`$VISUAL`/`nano`/`vi` (creating it from `DEFAULT_CONFIG_TOML` first if missing), then re-parses it with `FileConfig::load` after the editor exits and reports whether it's valid. Does not restart the service.

`packaging/systemd/install.sh` is the standalone shell alternative to `--install-systemd`. `packaging/systemd/service-status.sh` is a separate, richer JSON status helper: it parses `systemctl status`, includes process details, performs the TCP reachability checks supported by the script, collects recent journal errors, and reads the active-connection snapshot. It is not equivalent to the built-in raw `--service-status` passthrough. See `packaging/systemd/README.md`.
