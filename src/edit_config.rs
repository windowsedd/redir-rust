//! `-e` / `--edit-config`: opens the config file in `$EDITOR`, creating it
//! from the built-in default first if it doesn't exist yet, then re-parses
//! it after the editor exits so a typo doesn't go unnoticed until the next
//! restart. Does not touch the running service.

use std::io;
use std::path::Path;
use std::process::{Command, ExitCode};

use crate::config::{FileConfig, DEFAULT_CONFIG_TOML};

pub fn run(path: &Path) -> ExitCode {
    if let Err(err) = ensure_exists(path) {
        eprintln!(
            "error: failed to create default config at {}: {err}",
            path.display()
        );
        return ExitCode::FAILURE;
    }

    match launch_editor(path) {
        Ok(EditorOutcome::Ran(status)) if !status.success() => {
            eprintln!("warning: editor exited with {status}");
        }
        Ok(EditorOutcome::Ran(_)) => {}
        Ok(EditorOutcome::NoneFound(tried)) => {
            eprintln!(
                "error: no editor found; set $EDITOR (tried: {})",
                tried.join(", ")
            );
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("error: failed to launch editor: {err}");
            return ExitCode::FAILURE;
        }
    }

    match FileConfig::load(path) {
        Ok(config) => println!("config OK ({} redirect(s))", config.redirects.len()),
        Err(err) => eprintln!("warning: config has an error: {err}"),
    }

    ExitCode::SUCCESS
}

fn ensure_exists(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DEFAULT_CONFIG_TOML)?;
    println!("created default config at {}", path.display());
    Ok(())
}

enum EditorOutcome {
    Ran(std::process::ExitStatus),
    NoneFound(Vec<String>),
}

fn editor_candidates() -> Vec<String> {
    let mut candidates = Vec::new();
    if let Ok(editor) = std::env::var("EDITOR") {
        if !editor.is_empty() {
            candidates.push(editor);
        }
    }
    if let Ok(editor) = std::env::var("VISUAL") {
        if !editor.is_empty() {
            candidates.push(editor);
        }
    }
    #[cfg(unix)]
    {
        candidates.push("nano".to_string());
        candidates.push("vi".to_string());
    }
    #[cfg(windows)]
    {
        candidates.push("notepad".to_string());
    }
    candidates
}

/// Tries each candidate editor in order, only falling through to the next
/// one when the current one isn't installed (`NotFound`) -- a real launch
/// failure (e.g. permission denied) is surfaced immediately instead of
/// being masked by silently trying the next candidate.
fn launch_editor(path: &Path) -> io::Result<EditorOutcome> {
    let candidates = editor_candidates();
    for editor in &candidates {
        match Command::new(editor).arg(path).status() {
            Ok(status) => return Ok(EditorOutcome::Ran(status)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(EditorOutcome::NoneFound(candidates))
}
