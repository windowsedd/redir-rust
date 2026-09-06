use std::process::Command;

/// Emits build-time metadata consumed by `--version`.
///
/// Everything here is best-effort: a source tarball with no git checkout, or
/// a machine without `git`/`rustc` on PATH, still builds — the corresponding
/// field just reads "unknown".
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Rebuild when HEAD moves so the embedded commit stays accurate.
    for path in [".git/HEAD", ".git/refs/heads"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    let commit =
        run("git", &["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = matches!(
        run("git", &["status", "--porcelain", "--untracked-files=no"]),
        Some(s) if !s.is_empty()
    );
    let commit = if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    };

    let rustc = run("rustc", &["--version"]).unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=REDIR_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=REDIR_RUSTC={rustc}");
    println!(
        "cargo:rustc-env=REDIR_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".into())
    );
    println!(
        "cargo:rustc-env=REDIR_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into())
    );
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
