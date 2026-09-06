//! Release version strings for `-V` / `--version`.
//!
//! `build.rs` embeds the git commit, target triple, profile and rustc version
//! at compile time; each is best-effort and falls back to "unknown" so a
//! build outside a git checkout still works. Everything here is a `const`
//! so it can be handed straight to clap's `version`/`long_version`.

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git commit the binary was built from, suffixed `-dirty` when the
/// working tree had uncommitted tracked changes. "unknown" outside git.
pub const GIT_COMMIT: &str = env!("REDIR_GIT_COMMIT");

/// Target triple the binary was compiled for.
pub const TARGET: &str = env!("REDIR_TARGET");

/// Cargo profile ("debug"/"release") used for the build.
pub const PROFILE: &str = env!("REDIR_PROFILE");

/// `rustc --version` of the compiler that built the binary.
pub const RUSTC: &str = env!("REDIR_RUSTC");

/// Single-line version, used for `-V`.
pub const SHORT: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("REDIR_GIT_COMMIT"),
    " ",
    env!("REDIR_PROFILE"),
    ")"
);

/// Multi-line release details, used for `--version`.
pub const LONG: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\ncommit:  ",
    env!("REDIR_GIT_COMMIT"),
    "\ntarget:  ",
    env!("REDIR_TARGET"),
    "\nprofile: ",
    env!("REDIR_PROFILE"),
    "\nrustc:   ",
    env!("REDIR_RUSTC")
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_version_starts_with_the_crate_version() {
        assert!(SHORT.starts_with(VERSION));
        assert!(SHORT.contains(GIT_COMMIT));
    }

    #[test]
    fn long_version_lists_every_build_field() {
        for field in ["commit:", "target:", "profile:", "rustc:"] {
            assert!(LONG.contains(field), "missing {field} in:\n{LONG}");
        }
        assert!(LONG.contains(TARGET));
        assert!(LONG.contains(RUSTC));
    }
}
