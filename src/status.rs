//! `--status`/`--service-status`: prints the real `systemctl status` output
//! for the unit, as-is, so you see exactly what you'd get running it
//! directly, without an extra terminal or remembering the unit name.

use std::process::ExitCode;
#[cfg(not(windows))]
use std::process::{Command, Output};

/// Quiet state query for the menu; unavailable service managers remain unknown.
#[cfg(not(windows))]
pub fn state(unit: &str) -> std::io::Result<String> {
    let output = Command::new("systemctl")
        .args(["is-active", unit])
        .output()?;
    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(match state.as_str() {
        "active" => "running".into(),
        "inactive" => "stopped".into(),
        "failed" | "activating" | "deactivating" | "reloading" => state,
        _ => "unknown".into(),
    })
}

#[cfg(windows)]
pub fn state(_unit: &str) -> std::io::Result<String> {
    Ok(if crate::windows_process::running()? {
        "running"
    } else {
        "stopped"
    }
    .into())
}

#[cfg(not(windows))]
pub fn run(unit: &str) -> ExitCode {
    match Command::new("systemctl")
        .args(["status", "--no-pager", "-l", unit])
        .output()
    {
        Ok(Output {
            stdout,
            stderr,
            status,
        }) => {
            print!("{}", String::from_utf8_lossy(&stdout));
            eprint!("{}", String::from_utf8_lossy(&stderr));
            if status.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            eprintln!("error: failed to run systemctl: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
pub fn run(_unit: &str) -> ExitCode {
    match crate::windows_process::status() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
