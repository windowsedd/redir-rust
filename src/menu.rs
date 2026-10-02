use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use redir_rust::{config_manager, service_ctl, status};

use crate::setup;

const ITEMS: [&str; 9] = [
    "Start",
    "Stop",
    "Setup Config",
    "Edit Config",
    "Status",
    "Monitor",
    "Open GUI",
    "Reload",
    "Exit",
];

const DESCRIPTIONS: [&str; 9] = [
    "Start the service",
    "Stop the service",
    "Add a redirect",
    "Change a redirect",
    "Inspect the service",
    "Watch traffic",
    "Open browser manager",
    "Apply config without disconnecting clients",
    "",
];

fn running_counts(stats: &str, now_ms: u64) -> Option<(usize, u64)> {
    let stats: serde_json::Value = serde_json::from_str(stats).ok()?;
    let timestamp = stats["ts_ms"].as_u64()?;
    if now_ms.checked_sub(timestamp)? > 5_000 {
        return None;
    }
    let redirects = stats["redirects"].as_array()?;
    let connections = redirects.iter().fold(0u64, |total, r| {
        total.saturating_add(r["connections"].as_u64().unwrap_or(0))
    });
    Some((redirects.len(), connections))
}

fn service_summary() -> String {
    let state = status::state("redir-rust.service").unwrap_or_else(|_| "unknown".into());
    let counts = match state.as_str() {
        "stopped" | "failed" => Some((0, 0)),
        "running" | "reloading" => std::fs::read_to_string(redir_rust::connections::STATS_FILE)
            .ok()
            .and_then(|stats| {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
                running_counts(&stats, now.as_millis() as u64)
            }),
        _ => None,
    };
    let summary = counts.map_or_else(
        || "Redirects unknown    Connections unknown".into(),
        |(redirects, connections)| {
            format!(
                "{redirects} redirect{}    {connections} connection{}",
                if redirects == 1 { "" } else { "s" },
                if connections == 1 { "" } else { "s" }
            )
        },
    );
    let state = match state.as_str() {
        "running" => "● Running",
        "stopped" => "○ Stopped",
        "failed" => "● Failed",
        "activating" => "○ Starting",
        "deactivating" => "○ Stopping",
        "reloading" => "○ Reloading",
        _ => "○ Unknown",
    };
    format!("{state} · {summary}")
}

fn menu_title() -> String {
    format!(
        "redir-rust v{}\nTCP / UDP redirect manager · {}\n\n{}",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_AUTHORS"),
        service_summary(),
    )
}

/// Clack renders the terminal menu; status is refreshed each time it opens.
fn select_with_clack() -> io::Result<Option<usize>> {
    let mut prompt = crate::prompts::select("What would you like to do?")?;
    cliclack::intro(format!("redir-rust · v{}", env!("CARGO_PKG_VERSION")))?;
    cliclack::note(
        format!("TCP / UDP redirect manager · {}", env!("CARGO_PKG_AUTHORS")),
        service_summary(),
    )?;
    for (index, (label, description)) in ITEMS.iter().zip(DESCRIPTIONS).enumerate() {
        prompt = prompt.item(index, label, description);
    }
    match prompt.interact() {
        Ok(index) => Ok((index != ITEMS.len() - 1).then_some(index)),
        Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(None),
        Err(err) => Err(err),
    }
}

pub fn run() -> ExitCode {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    loop {
        let choice = if interactive {
            select_with_clack()
        } else {
            select_with_number()
        };
        let choice = match choice {
            Ok(Some(choice)) => choice,
            Ok(None) => {
                if interactive {
                    let _ = cliclack::outro("Goodbye");
                }
                return ExitCode::SUCCESS;
            }
            Err(err) => {
                eprintln!("error: failed to read menu choice: {err}");
                return ExitCode::FAILURE;
            }
        };
        if interactive {
            if let Err(err) = cliclack::clear_screen() {
                eprintln!("error: failed to clear terminal: {err}");
                return ExitCode::FAILURE;
            }
        }
        match choice {
            0 => {
                service_ctl::run("start", "redir-rust.service");
            }
            1 => {
                service_ctl::run("stop", "redir-rust.service");
            }
            2 => match config_path() {
                Ok(path) => {
                    if let Err(err) = setup::run(&path) {
                        eprintln!("error: setup failed: {err}");
                    }
                }
                Err(err) => eprintln!("error: {err}"),
            },
            3 => match config_path() {
                Ok(path) => {
                    if let Err(err) = setup::edit(&path) {
                        eprintln!("error: edit failed: {err}");
                    }
                }
                Err(err) => eprintln!("error: {err}"),
            },
            4 => {
                status::run("redir-rust.service");
            }
            5 => {
                return redir_rust::monitor::run();
            }
            6 => {
                return match config_path() {
                    Ok(path) => crate::gui::run(&path, "0.0.0.0:0".parse().unwrap()),
                    Err(err) => {
                        eprintln!("error: {err}");
                        ExitCode::FAILURE
                    }
                };
            }
            7 => match config_path() {
                Ok(path) => match redir_rust::reload::request(&path) {
                    Ok(()) => println!("Configuration reloaded; established sessions retained."),
                    Err(err) => eprintln!("error: reload failed: {err}"),
                },
                Err(err) => eprintln!("error: {err}"),
            },
            _ => return ExitCode::SUCCESS,
        }
        if interactive {
            match crate::prompts::select("Next action").and_then(|prompt| {
                prompt
                    .item(true, "⬅ Previous", "Main menu")
                    .item(false, "Exit", "")
                    .interact()
            }) {
                Ok(true) => {}
                Ok(false) => {
                    let _ = cliclack::outro("Goodbye");
                    return ExitCode::SUCCESS;
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    let _ = cliclack::outro("Goodbye");
                    return ExitCode::SUCCESS;
                }
                Err(err) => {
                    eprintln!("error: failed to read menu choice: {err}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }
}

/// Numbered prompt used when stdin/stdout is not a terminal.
fn select_with_number() -> io::Result<Option<usize>> {
    loop {
        println!("\n{}\n", menu_title());
        for (index, (label, description)) in ITEMS.iter().zip(DESCRIPTIONS).enumerate() {
            println!("  {}  {label:<14} {description}", index + 1);
        }
        print!("Select [1-9]: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer)? == 0 {
            return Ok(None);
        }
        match answer.trim().parse::<usize>() {
            Ok(n) if (1..=ITEMS.len()).contains(&n) => return Ok(Some(n - 1)),
            _ => eprintln!("Choose a number from 1 to 9."),
        }
    }
}

fn config_path() -> std::io::Result<std::path::PathBuf> {
    config_manager::configured_path(&config_manager::settings_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_live_redirects_and_rejects_unavailable_snapshots() {
        let stats = r#"{"ts_ms":10000,"redirects":[{"name":"java","connections":2},{"name":"bedrock","connections":3}]}"#;
        assert_eq!(running_counts(stats, 12000), Some((2, 5)));
        assert_eq!(running_counts(stats, 16000), None);
        assert_eq!(running_counts(stats, 9000), None);
        assert_eq!(running_counts("{}", 12000), None);
        assert_eq!(running_counts("invalid", 12000), None);
        assert_eq!(
            running_counts(r#"{"ts_ms":10000,"redirects":[]}"#, 12000),
            Some((0, 0))
        );
    }
}
