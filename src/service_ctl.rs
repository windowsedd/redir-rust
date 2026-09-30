//! `--start` / `--stop` / `--restart`: thin `systemctl` wrappers, so day-to-day
//! service control doesn't require remembering the unit name/typing it out.
//! Inherits stdio, so `systemctl`'s own output (including permission errors)
//! shows through as-is -- same root requirement `systemctl` always enforces.

#[cfg(not(windows))]
use std::process::Command;
use std::process::ExitCode;

#[cfg(not(windows))]
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

#[cfg(windows)]
pub fn run(action: &str, _unit: &str) -> ExitCode {
    let result = match action {
        "start" => crate::windows_process::start(),
        "stop" => crate::windows_process::stop(),
        "restart" => crate::windows_process::stop().and_then(|_| crate::windows_process::start()),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unknown action",
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
