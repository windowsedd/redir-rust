//! Rewrites this process's own argv memory so `/proc/pid/cmdline` (and
//! therefore `ps`, `systemctl status`'s `CGroup:` process listing, etc.)
//! shows live connection info instead of the static command line. This is
//! the same technique nginx/postgres use ("process title spoofing").
//! Linux-only; a no-op everywhere else.
//!
//! Unlike `prctl(PR_SET_NAME)` (which only changes `/proc/pid/comm`, capped
//! at 15 bytes, and does *not* affect `/proc/pid/cmdline`), this actually
//! overwrites the argv bytes in place. The bounds of that region
//! (`arg_start`/`arg_end`) are read from `/proc/self/stat`, which the
//! kernel maintains itself -- so we know exactly how many bytes we're
//! allowed to touch, and can never write past the region the kernel
//! originally reserved for argv.

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::{LazyLock, Mutex};

    static ARG_BOUNDS: LazyLock<Option<(usize, usize)>> = LazyLock::new(read_arg_bounds);
    static WRITE_LOCK: Mutex<()> = Mutex::new(());

    pub fn set(title: &str) {
        let Some((start, end)) = *ARG_BOUNDS else {
            return;
        };
        let len = end.saturating_sub(start);
        if len == 0 {
            return;
        }

        let _guard = WRITE_LOCK.lock().unwrap();
        // SAFETY: `start`/`end` come from the kernel's own record of this
        // process's argv region (/proc/self/stat's arg_start/arg_end),
        // which is writable memory owned by this process for its entire
        // lifetime. `n` is capped to `len - 1`, so we never write past
        // `end`, and the final byte is always left as (or set to) NUL.
        unsafe {
            let slice = std::slice::from_raw_parts_mut(start as *mut u8, len);
            let bytes = title.as_bytes();
            let n = bytes.len().min(len - 1);
            slice[..n].copy_from_slice(&bytes[..n]);
            for b in &mut slice[n..] {
                *b = 0;
            }
        }
    }

    /// Parses `arg_start`/`arg_end` out of `/proc/self/stat`. Field 2
    /// (`comm`) is parenthesized and may itself contain spaces or
    /// parentheses, so we locate it by its *last* `)` rather than naive
    /// whitespace splitting. Field numbering per `proc(5)`: field 3 is the
    /// first field after `comm`, so field N (N >= 3) is at index `N - 3` in
    /// the remainder; `arg_start` is field 48, `arg_end` is field 49.
    pub(super) fn read_arg_bounds() -> Option<(usize, usize)> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let close_paren = stat.rfind(')')?;
        let rest = stat.get(close_paren + 2..)?;
        let fields: Vec<&str> = rest.split_whitespace().collect();

        let arg_start: usize = fields.get(45)?.parse().ok()?;
        let arg_end: usize = fields.get(46)?.parse().ok()?;
        (arg_end > arg_start).then_some((arg_start, arg_end))
    }
}

#[cfg(target_os = "linux")]
pub fn set(title: &str) {
    linux::set(title);
}

#[cfg(not(target_os = "linux"))]
pub fn set(_title: &str) {}

#[cfg(test)]
mod tests {
    // `set` mutates this whole process's argv memory -- shared, global
    // state -- and `cargo test` runs test functions in parallel threads by
    // default. Without this, `set_does_not_panic` and
    // `set_updates_proc_self_cmdline` race: one test's `set()` call can land
    // between the other's `set()` and its `/proc/self/cmdline` read, making
    // the assertion see the wrong test's string. Every test that calls
    // `set` takes this lock first to force them to run one at a time.
    static SET_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn set_does_not_panic() {
        let _guard = SET_TEST_LOCK.lock().unwrap();
        // Cross-platform smoke test: on non-Linux this is a no-op, on Linux
        // it exercises the real argv-rewrite path (see the Linux-specific
        // test below for behavioral assertions).
        super::set("redir-rust test title");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn arg_bounds_are_sane_for_this_process() {
        let bounds = super::linux::read_arg_bounds();
        let Some((start, end)) = bounds else {
            // Some environments (e.g. certain containers/sandboxes) may
            // restrict /proc access; degrading to a no-op is the documented
            // behavior, not a bug, so don't fail the test for that.
            return;
        };
        assert!(
            end > start,
            "arg_end ({end}) should be after arg_start ({start})"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn set_updates_proc_self_cmdline() {
        let _guard = SET_TEST_LOCK.lock().unwrap();
        let marker = "redir-rust-proctitle-test-marker";
        super::set(marker);

        let cmdline = std::fs::read_to_string("/proc/self/cmdline").unwrap_or_default();
        if cmdline.is_empty() {
            // Same reasoning as above: missing /proc access degrades silently.
            return;
        }
        assert!(
            cmdline.contains(marker),
            "expected {marker:?} in /proc/self/cmdline, got {cmdline:?}"
        );
    }
}
