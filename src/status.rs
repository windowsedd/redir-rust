//! `--status`/`--service-status`: prints the real `systemctl status` output
//! for the unit, as-is, so you see exactly what you'd get running it
//! directly, without an extra terminal or remembering the unit name.

use std::process::{Command, ExitCode, Output};

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
