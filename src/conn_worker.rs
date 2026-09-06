//! Runs one connection's actual bidirectional byte copy in its own child
//! process (via `std::thread`, not async -- the child handles exactly one
//! connection for its entire life, so plain blocking threads are simplest),
//! instead of an in-process async task. The parent (`proxy.rs`) still does
//! everything up to the point a connection is ready to be proxied itself --
//! `Plugin::on_connect`, the connect-to-target attempt, and
//! `Plugin::on_target_failure` (e.g. the Minecraft "server offline" MOTD) --
//! and only hands the two sockets off here once it knows real proxying is
//! about to happen. This is what gives each connection its own PID (and so
//! its own line under `systemctl status`'s `CGroup:` tree), unlike the
//! marker-only approach this replaces, which kept all data transfer in one
//! process and only spawned a child for display.
//!
//! Trade-offs vs. the async-task-per-connection model this replaces:
//! - Every connection now costs a fork+exec, not a cheap tokio spawn.
//! - `Plugin::on_data_receive` (mid-stream rewriting) cannot run in the
//!   child; no built-in plugin uses that hook today, but a future one that
//!   does would need to run in the parent instead (see the `cfg(not(unix))`
//!   fallback below, which still supports it).
//! - UDP (`udp_proxy.rs`) is unaffected; per-client processes aren't a
//!   natural fit there since all clients share one bound socket.

/// Checked in plain `fn main`, before clap parsing and before a tokio
/// runtime is started: is this invocation the hidden per-connection worker
/// mode (`redir-rust --conn-worker`)? The worker should stay as cheap and
/// simple as possible since it's spawned once per connection.
pub fn is_worker_invocation() -> bool {
    std::env::args().nth(1).as_deref() == Some("--conn-worker")
}

#[cfg(unix)]
pub use unix::{run, run_worker};

/// Non-Unix fallback: no per-connection child processes (fd-passing via
/// `Stdio::from(OwnedFd)` is a Unix-only mechanism), so this just keeps
/// doing the original in-process async copy, plugins included.
#[cfg(not(unix))]
pub async fn run(
    client: tokio::net::TcpStream,
    target: tokio::net::TcpStream,
    plugins: &[std::sync::Arc<dyn crate::plugin::Plugin>],
    shaping: Option<&crate::shaping::ShapingConfig>,
    _worker_executable: Option<&std::path::Path>,
) -> std::io::Result<()> {
    crate::proxy::pipe(client, target, plugins, shaping).await
}

#[cfg(not(unix))]
pub fn run_worker() -> std::process::ExitCode {
    // Unreachable in practice: `run` above never re-execs on non-Unix, so
    // `is_worker_invocation()` never leads here.
    std::process::ExitCode::FAILURE
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::net::Shutdown;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::{ExitCode, Stdio};
    use std::sync::Arc;
    use std::thread;

    use tokio::net::TcpStream;
    use tokio::process::Command;

    use crate::plugin::Plugin;
    use crate::shaping::ShapingConfig;

    /// Hands `client` and `target` off to a freshly spawned child process
    /// that does the actual bidirectional copy, then waits for it to exit.
    /// Both sockets are passed as the child's stdin/stdout (fd 0/1) -- each
    /// is already a full-duplex TCP socket, so one fd per side is enough;
    /// the child needs nothing beyond "copy bytes between these two fds".
    /// `shaping`, if active, is passed as extra CLI args (see
    /// `parse_shaping_args`) since the child has no other side channel to
    /// the parent's config.
    pub async fn run(
        client: TcpStream,
        target: TcpStream,
        _plugins: &[Arc<dyn Plugin>],
        shaping: Option<&ShapingConfig>,
        worker_executable: Option<&std::path::Path>,
    ) -> io::Result<()> {
        let client_fd: OwnedFd = client.into_std()?.into();
        let target_fd: OwnedFd = target.into_std()?.into();

        let mut cmd = Command::new(worker_exe_path(worker_executable)?);
        cmd.arg("--conn-worker");
        if let Some(shaping) = shaping {
            if let Some(bps) = shaping.max_bandwidth_bps {
                cmd.arg("--max-bandwidth-bps").arg(bps.to_string());
            }
            if let Some(ms) = shaping.random_wait_ms {
                cmd.arg("--random-wait-ms").arg(ms.to_string());
            }
            cmd.arg("--wait-in-out").arg(shaping.wait_in_out.as_str());
            cmd.arg("--bufsize").arg(shaping.bufsize.to_string());
        }
        cmd.stdin(Stdio::from(client_fd));
        cmd.stdout(Stdio::from(target_fd));
        // Left at the default (inherit): if the worker panics, the message
        // goes to the same journal as the parent's own logs instead of
        // being silently discarded.

        let mut child = cmd.spawn()?;
        let status = child.wait().await?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "conn-worker child exited with {status}"
            )));
        }
        Ok(())
    }

    /// Reconstructs a `ShapingConfig` from the `--max-bandwidth-bps`/
    /// `--random-wait-ms`/`--wait-in-out`/`--bufsize` args `run` passes to
    /// the re-exec'd worker. Deliberately not using `clap` here: the worker
    /// mode is detected and dispatched before `Cli::parse()` ever runs (see
    /// `is_worker_invocation`), so this stays a small manual parser instead
    /// of pulling the whole CLI definition into this hot, minimal path.
    /// Returns `None` (no shaping, plain fast copy) when `args` is empty --
    /// meaning the parent didn't pass any (see `run`, which only ever sends
    /// these args when `shaping` was `Some`) -- or on any parse failure;
    /// silently falling back rather than failing every connection over a
    /// malformed arg is the safer default.
    fn parse_shaping_args(args: &[String]) -> Option<ShapingConfig> {
        if args.is_empty() {
            return None;
        }

        let mut max_bandwidth_bps = None;
        let mut random_wait_ms = None;
        let mut wait_in_out = crate::shaping::WaitInOut::Both;
        let mut bufsize = 16 * 1024;

        let mut i = 0;
        while i < args.len() {
            let (flag, value) = (args.get(i)?.as_str(), args.get(i + 1)?.as_str());
            match flag {
                "--max-bandwidth-bps" => max_bandwidth_bps = Some(value.parse().ok()?),
                "--random-wait-ms" => random_wait_ms = Some(value.parse().ok()?),
                "--wait-in-out" => wait_in_out = value.parse().ok()?,
                "--bufsize" => bufsize = value.parse().ok()?,
                _ => return None,
            }
            i += 2;
        }

        Some(ShapingConfig {
            max_bandwidth_bps,
            wait_in_out,
            random_wait_ms,
            bufsize,
        })
    }

    /// Path to the binary to re-exec as the per-connection worker. Normally
    /// this process's own binary (`current_exe()`), so the running
    /// `redir-rust` process re-execs itself. An explicit per-proxy override
    /// takes precedence. The environment fallback remains for compatibility
    /// with existing deployments and ad-hoc testing.
    fn worker_exe_path(override_path: Option<&std::path::Path>) -> io::Result<std::path::PathBuf> {
        if let Some(path) = override_path {
            return Ok(path.to_path_buf());
        }
        if let Ok(path) = std::env::var("REDIR_RUST_WORKER_EXE") {
            return Ok(std::path::PathBuf::from(path));
        }
        std::env::current_exe()
    }

    #[cfg(test)]
    mod tests {
        use super::worker_exe_path;

        #[test]
        fn explicit_worker_executable_takes_precedence() {
            let path = std::path::Path::new("/explicit/worker");
            assert_eq!(worker_exe_path(Some(path)).unwrap(), path);
        }
    }

    /// Entry point for the worker child: copies bytes bidirectionally
    /// between fd 0 (client) and fd 1 (target) until both directions hit
    /// EOF, then exits.
    ///
    /// Every outcome (byte counts, errors, whether fd 0/1 are even valid
    /// connected sockets) is written to stderr, which inherits to the same
    /// journal as the parent's logs -- this is deliberately noisy (one line
    /// per connection) while we're tracking down why connections proxied
    /// through this path aren't relaying any data; trim it back down once
    /// that's confirmed fixed.
    pub fn run_worker() -> ExitCode {
        // SAFETY: this mode is only ever reached via `run` above re-
        // exec'ing this same binary with fd 0/1 set to the client/target
        // sockets (see the `Stdio::from` calls there); it's never invoked
        // any other way, so fd 0/1 are guaranteed to be those sockets, each
        // owned exclusively by this process.
        let client = unsafe { std::net::TcpStream::from_raw_fd(0) };
        let target = unsafe { std::net::TcpStream::from_raw_fd(1) };

        // These fds started out as tokio (async, non-blocking) sockets;
        // that O_NONBLOCK flag belongs to the underlying open file
        // description, so it survives being passed through OwnedFd/Stdio
        // and across fork+exec unchanged. Force blocking mode explicitly
        // rather than relying on the parent to have already done it --
        // `io::copy` below uses plain blocking reads/writes and has no
        // retry logic for `WouldBlock`, so a non-blocking fd here makes
        // every read/write that doesn't have data instantly ready fail
        // immediately with EAGAIN instead of waiting for the peer.
        if let Err(err) = client.set_nonblocking(false) {
            eprintln!("conn-worker: failed to set client socket blocking: {err}");
            return ExitCode::FAILURE;
        }
        if let Err(err) = target.set_nonblocking(false) {
            eprintln!("conn-worker: failed to set target socket blocking: {err}");
            return ExitCode::FAILURE;
        }

        match (client.peer_addr(), target.peer_addr()) {
            (Ok(client_addr), Ok(target_addr)) => {
                crate::proctitle::set(&format!("redir-rust {client_addr} -> {target_addr}"));
            }
            (client_res, target_res) => {
                eprintln!(
                    "conn-worker: fd 0/1 are not valid connected sockets (client peer_addr: {client_res:?}, target peer_addr: {target_res:?})"
                );
            }
        }

        // Args after "--conn-worker" carry shaping settings, if any (see
        // `run`/`parse_shaping_args`); `None` means an unshaped connection,
        // which uses a plain `io::copy` fast path below.
        let shaping_args: Vec<String> = std::env::args().skip(2).collect();
        let shaping = parse_shaping_args(&shaping_args);

        let (mut client_writer, mut target_writer) = match (client.try_clone(), target.try_clone())
        {
            (Ok(c), Ok(t)) => (c, t),
            (c, t) => {
                eprintln!("conn-worker: failed to clone sockets (client: {c:?}, target: {t:?})");
                return ExitCode::FAILURE;
            }
        };
        let mut client_reader = client;
        let mut target_reader = target;

        let shaping_for_c2t = shaping.clone();
        let client_to_target = thread::spawn(move || -> io::Result<u64> {
            let n = crate::shaping::shaped_copy_blocking(
                &mut client_reader,
                &mut target_writer,
                crate::plugin::Direction::ClientToTarget,
                shaping_for_c2t.as_ref(),
            )?;
            target_writer.shutdown(Shutdown::Write).ok();
            Ok(n)
        });
        let target_to_client = thread::spawn(move || -> io::Result<u64> {
            let n = crate::shaping::shaped_copy_blocking(
                &mut target_reader,
                &mut client_writer,
                crate::plugin::Direction::TargetToClient,
                shaping.as_ref(),
            )?;
            client_writer.shutdown(Shutdown::Write).ok();
            Ok(n)
        });

        let mut ok = true;
        match client_to_target.join() {
            Ok(Ok(n)) => eprintln!("conn-worker: client->target copied {n} bytes"),
            Ok(Err(err)) => {
                eprintln!("conn-worker: client->target copy failed: {err}");
                ok = false;
            }
            Err(_) => ok = false, // panic (rare: panic="abort" would already have killed the process)
        }
        match target_to_client.join() {
            Ok(Ok(n)) => eprintln!("conn-worker: target->client copied {n} bytes"),
            Ok(Err(err)) => {
                eprintln!("conn-worker: target->client copy failed: {err}");
                ok = false;
            }
            Err(_) => ok = false,
        }

        if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }
}
