//! `--start` / `--stop` / `--restart`: thin `systemctl` wrappers, so day-to-day
//! service control doesn't require remembering the unit name/typing it out.
//! Inherits stdio, so `systemctl`'s own output (including permission errors)
//! shows through as-is -- same root requirement `systemctl` always enforces.

use std::process::{Command, ExitCode};

pub fn run(action: &str, unit: &str) -> ExitCode {
    match Command::new("systemctl").arg(action).arg(unit).status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => {
            eprintln!("systemctl {action} {unit} failed: {status}");
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("failed to run systemctl: {err}");
            ExitCode::FAILURE
        }
    }
}
