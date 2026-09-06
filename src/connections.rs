//! Process-wide registry of currently-open connections, so
//! `--service-status` (run as a separate, short-lived process) can report
//! "who's connected right now" for the actual running redirector.
//!
//! Every redirect (`proxy.rs` for TCP, `udp_proxy.rs` for UDP) holds a
//! [`ConnectionGuard`] for as long as a connection/session is open; the
//! guard's `Drop` impl deregisters it, so every exit path (normal close,
//! error, panic-unwind) stays consistent without extra bookkeeping at each
//! return site. The registry is snapshotted to a JSON file on every change
//! so a separate `--service-status` invocation can read it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Where the live snapshot is written. Lives under the `RuntimeDirectory`
/// systemd creates for the unit (world-readable by default), so
/// `--service-status` can read it regardless of which user invokes it.
pub const STATE_FILE: &str = "/run/redir-rust/connections.json";
const STATE_DIR: &str = "/run/redir-rust";

struct Entry {
    redirect: String,
    protocol: &'static str,
    client: SocketAddr,
    target: SocketAddr,
    connected_at: Instant,
    connected_at_unix: u64,
}

struct Registry {
    next_id: AtomicU64,
    entries: Mutex<HashMap<u64, Entry>>,
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| Registry {
    next_id: AtomicU64::new(1),
    entries: Mutex::new(HashMap::new()),
});

/// Locks the registry, recovering from a poisoned mutex instead of
/// propagating the panic. The guarded value is a plain map with no
/// invariants to violate, and this lock is taken on every connect and
/// disconnect: if one panicking thread could poison it permanently, a single
/// unrelated panic would take down connection tracking for the whole
/// process.
fn entries() -> std::sync::MutexGuard<'static, HashMap<u64, Entry>> {
    REGISTRY
        .entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// RAII handle for a tracked connection: hold it for as long as the
/// connection is open. Dropping it (however the caller returns) removes the
/// entry from the live registry and refreshes the on-disk snapshot.
#[must_use]
pub struct ConnectionGuard(u64);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        entries().remove(&self.0);
        persist();
    }
}

pub fn track(
    redirect: &str,
    protocol: &'static str,
    client: SocketAddr,
    target: SocketAddr,
) -> ConnectionGuard {
    let id = REGISTRY.next_id.fetch_add(1, Ordering::Relaxed);
    let connected_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    entries().insert(
        id,
        Entry {
            redirect: redirect.to_string(),
            protocol,
            client,
            target,
            connected_at: Instant::now(),
            connected_at_unix,
        },
    );
    persist();

    ConnectionGuard(id)
}

/// Caps how many client addresses get spelled out in the sd_notify status
/// line -- meant to stay a short one-liner, not grow unbounded with
/// traffic. Per-connection detail beyond this belongs to the `CGroup:` tree
/// (see `conn_worker.rs`) or to `--service-status`'s JSON, not here.
const STATUS_LINE_CLIENT_LIMIT: usize = 8;

/// Snapshots the registry to `STATE_FILE`, and pushes the same summary to
/// the sd_notify status line (`systemctl status`'s `Status:` field).
/// Deliberately does *not* touch the process title: for TCP, each
/// connection already gets its own child PID under `CGroup:` (see
/// `conn_worker.rs`), so re-stamping the parent's title with the same
/// summary was redundant and, once truncated across the original argv
/// buffer, showed up as a trail of empty `""` arguments in `systemctl
/// status`. Best-effort throughout: a permission or missing-directory
/// failure here must never take down an active redirect, so all errors are
/// swallowed (this is also why it degrades silently when run manually as a
/// non-root user, or on non-Unix).
fn persist() {
    #[cfg(unix)]
    {
        let (snapshot, status_text) = {
            let entries = entries();
            let snapshot: Vec<serde_json::Value> = entries
                .iter()
                .map(|(id, e)| {
                    serde_json::json!({
                        "id": id,
                        "redirect": e.redirect,
                        "protocol": e.protocol,
                        "client": e.client.to_string(),
                        "target": e.target.to_string(),
                        "connected_at_unix": e.connected_at_unix,
                        "duration_secs": e.connected_at.elapsed().as_secs(),
                    })
                })
                .collect();

            let shown: Vec<SocketAddr> = entries
                .values()
                .take(STATUS_LINE_CLIENT_LIMIT)
                .map(|e| e.client)
                .collect();
            let status_text = format_status_text(entries.len(), &shown);

            (snapshot, status_text)
        };

        crate::notify::status(&status_text);
        let _ = write_atomic(&snapshot);
    }
}

/// Builds the one-line summary pushed to the sd_notify status line: `total`
/// is the true connection count, `shown` is the (possibly truncated to
/// `STATUS_LINE_CLIENT_LIMIT`) slice of client addresses to spell out.
fn format_status_text(total: usize, shown: &[SocketAddr]) -> String {
    if total == 0 {
        return "online".to_string();
    }
    let parts: Vec<String> = shown.iter().map(|addr| format!("[{addr}]")).collect();
    let remaining = total.saturating_sub(shown.len());
    let suffix = if remaining > 0 {
        format!(" +{remaining} more")
    } else {
        String::new()
    };
    format!("{total} connection(s): {}{suffix}", parts.join(" "))
}

#[cfg(unix)]
fn write_atomic(snapshot: &[serde_json::Value]) -> std::io::Result<()> {
    use std::fs;

    fs::create_dir_all(STATE_DIR)?;
    let tmp_path = format!("{STATE_FILE}.tmp");
    fs::write(&tmp_path, serde_json::to_vec(snapshot).unwrap_or_default())?;
    fs::rename(&tmp_path, STATE_FILE)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_reports_online() {
        assert_eq!(format_status_text(0, &[]), "online");
    }

    #[test]
    fn all_clients_shown_when_under_limit() {
        let clients: Vec<SocketAddr> = vec![
            "127.0.0.1:1".parse().unwrap(),
            "127.0.0.1:2".parse().unwrap(),
        ];
        assert_eq!(
            format_status_text(2, &clients),
            "2 connection(s): [127.0.0.1:1] [127.0.0.1:2]"
        );
    }

    #[test]
    fn truncated_clients_get_remaining_count_suffix() {
        let clients: Vec<SocketAddr> = vec!["127.0.0.1:1".parse().unwrap()];
        assert_eq!(
            format_status_text(5, &clients),
            "5 connection(s): [127.0.0.1:1] +4 more"
        );
    }

    /// Whether `id` is currently registered. Deliberately checks one entry
    /// rather than the map's length: the registry is process-wide, so other
    /// tests running in parallel add and remove their own entries, and any
    /// assertion on the total count is a race. Reading into a `bool` also
    /// releases the lock before the assertion, so a failure here reports
    /// itself instead of poisoning the mutex for every other test.
    fn is_registered(id: u64) -> bool {
        entries().contains_key(&id)
    }

    #[test]
    fn track_and_drop_roundtrip_updates_registry() {
        let client: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let target: SocketAddr = "127.0.0.1:40001".parse().unwrap();

        let guard = track("test-redirect", "tcp", client, target);
        let id = guard.0;
        assert!(is_registered(id), "tracked connection should be registered");

        drop(guard);
        assert!(!is_registered(id), "dropping the guard should deregister it");
    }
}
