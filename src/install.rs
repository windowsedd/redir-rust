//! `--install-systemd`: sets this binary up as a systemd service in one
//! shot. Everything it needs (default config, unit file) is embedded in the
//! binary at compile time via `include_str!`, so a bare `scp`'d binary is
//! enough to self-install -- no separate script or repo checkout required
//! on the target host.

use std::process::ExitCode;

const BIN_NAME: &str = "redir-rust";
const INSTALL_BIN: &str = "/usr/local/bin/redir-rust";
const CONFIG_DIR: &str = "/etc/redir-rust";
const CONFIG_FILE: &str = "/etc/redir-rust/config.toml";
const UNIT_FILE: &str = "/etc/systemd/system/redir-rust.service";

const UNIT_TEMPLATE: &str = "\
[Unit]
Description=redir-rust port redirector
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
NotifyAccess=main
ExecStart=/usr/local/bin/redir-rust --config /etc/redir-rust/config.toml
Restart=on-failure
RestartSec=2
Environment=RUST_LOG=info
RuntimeDirectory=redir-rust

StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
";

#[cfg(unix)]
pub fn run() -> ExitCode {
    match unix::try_run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
pub fn run() -> ExitCode {
    eprintln!("error: --install-systemd is only supported on Linux (systemd)");
    ExitCode::FAILURE
}

/// Lighter-weight sibling of `--install-systemd` for the "I already have
/// this set up, just deploy the new binary" loop: copies the freshly built
/// binary to `/usr/local/bin` and restarts the unit, without touching the
/// config or unit file (so it won't clobber either if they've been hand-
/// edited since the initial install).
#[cfg(unix)]
pub fn update(unit: &str) -> ExitCode {
    match unix::try_update(unit) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
pub fn update(_unit: &str) -> ExitCode {
    eprintln!("error: --update is only supported on Linux (systemd)");
    ExitCode::FAILURE
}

#[cfg(unix)]
mod unix {
    use super::{BIN_NAME, CONFIG_DIR, CONFIG_FILE, INSTALL_BIN, UNIT_FILE, UNIT_TEMPLATE};
    use crate::config::DEFAULT_CONFIG_TOML;
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;

    pub fn try_run() -> io::Result<()> {
        println!("==> Installing binary to {INSTALL_BIN}");
        let current_exe = std::env::current_exe()?;
        if let Some(parent) = Path::new(INSTALL_BIN).parent() {
            fs::create_dir_all(parent).map_err(hint_sudo)?;
        }
        install_binary(&current_exe).map_err(hint_sudo)?;

        println!("==> Ensuring config at {CONFIG_FILE}");
        fs::create_dir_all(CONFIG_DIR).map_err(hint_sudo)?;
        if Path::new(CONFIG_FILE).exists() {
            println!("    already exists, leaving it alone");
        } else {
            fs::write(CONFIG_FILE, DEFAULT_CONFIG_TOML)?;
            println!("    installed default config -- edit {CONFIG_FILE} before relying on it");
        }

        println!("==> Installing unit file to {UNIT_FILE}");
        fs::write(UNIT_FILE, UNIT_TEMPLATE).map_err(hint_sudo)?;

        println!("==> Reloading systemd and (re)starting the service");
        run_cmd("systemctl", &["daemon-reload"])?;
        run_cmd("systemctl", &["enable", BIN_NAME])?;
        // `restart` (not `enable --now`) so re-running this after an
        // upgrade actually picks up the new binary -- `--now` is a no-op
        // "start" on an already-running unit, which would leave the old
        // process (and old binary) running until a manual restart.
        run_cmd("systemctl", &["restart", BIN_NAME])?;

        println!("==> Done");
        let _ = run_cmd("systemctl", &["status", "--no-pager", BIN_NAME]);

        Ok(())
    }

    pub fn try_update(unit: &str) -> io::Result<()> {
        println!("==> Installing binary to {INSTALL_BIN}");
        let current_exe = std::env::current_exe()?;
        install_binary(&current_exe).map_err(hint_sudo)?;

        println!("==> Restarting {unit}");
        run_cmd("systemctl", &["restart", unit])?;

        println!("==> Done");
        let _ = run_cmd("systemctl", &["status", "--no-pager", unit]);

        Ok(())
    }

    /// Copies `src` to `INSTALL_BIN` via a temp file + atomic rename in the
    /// same directory, rather than `fs::copy` directly onto `INSTALL_BIN`.
    /// Reinstalling over an already-running instance (e.g. re-running
    /// `--install-systemd` to upgrade, invoked as the already-installed
    /// `redir-rust` on $PATH) would otherwise fail with `ETXTBSY` ("Text
    /// file busy"): Linux refuses to open-and-truncate a binary that's
    /// currently mapped/executing. `rename()` just repoints the directory
    /// entry to a new inode, which works even while the old one is running.
    fn install_binary(src: &Path) -> io::Result<()> {
        let tmp_path = format!("{INSTALL_BIN}.new");
        fs::copy(src, &tmp_path)?;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755))?;
        fs::rename(&tmp_path, INSTALL_BIN)?;
        Ok(())
    }

    /// Permission-denied writes to system paths almost always mean "not
    /// root"; make that obvious instead of a bare "Access is denied" error.
    fn hint_sudo(err: io::Error) -> io::Error {
        if err.kind() == io::ErrorKind::PermissionDenied {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "permission denied -- rerun as root (sudo redir-rust --install-systemd)",
            )
        } else {
            err
        }
    }

    fn run_cmd(cmd: &str, args: &[&str]) -> io::Result<()> {
        let status = Command::new(cmd).args(args).status()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "{cmd} {} failed with {status}",
                args.join(" ")
            )));
        }
        Ok(())
    }
}
