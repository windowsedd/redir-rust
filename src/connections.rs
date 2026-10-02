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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Where the live snapshot is written. Lives under the `RuntimeDirectory`
/// systemd creates for the unit (world-readable by default), so
/// `--service-status` can read it regardless of which user invokes it.
pub const STATE_FILE: &str = "/run/redir-rust/connections.json";
/// Per-redirect lifetime totals plus a timestamp, written next to
/// `STATE_FILE` for `--monitor`. Kept as a separate file so the shape of
/// `STATE_FILE` (embedded verbatim by `service-status.sh`) stays an array.
pub const STATS_FILE: &str = "/run/redir-rust/stats.json";
const STATE_DIR: &str = "/run/redir-rust";

/// Byte counters for one connection. "up" is client -> target, "down" is
/// target -> client.
#[derive(Default)]
struct ConnCounters {
    up: AtomicU64,
    down: AtomicU64,
}

/// Lifetime totals for one redirect; unlike per-connection counters these
/// survive connections closing, so graphs don't dip when a client leaves.
struct Totals {
    protocol: Mutex<&'static str>,
    retired: AtomicBool,
    listen: Mutex<String>,
    up: AtomicU64,
    down: AtomicU64,
}

struct Entry {
    redirect: String,
    protocol: &'static str,
    client: SocketAddr,
    target: SocketAddr,
    connected_at: Instant,
    connected_at_unix: u64,
    counters: Arc<ConnCounters>,
}

struct Registry {
    next_id: AtomicU64,
    entries: Mutex<HashMap<u64, Entry>>,
}

static TOTALS: LazyLock<Mutex<HashMap<String, Arc<Totals>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(unix)]
static SNAPSHOT_WRITE: Mutex<()> = Mutex::new(());
static STARTED_MS: LazyLock<u64> = LazyLock::new(now_ms);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn totals_map() -> std::sync::MutexGuard<'static, HashMap<String, Arc<Totals>>> {
    TOTALS.lock().unwrap_or_else(|p| p.into_inner())
}

fn totals_for(redirect: &str, protocol: &'static str) -> Arc<Totals> {
    totals_map()
        .entry(redirect.to_string())
        .or_insert_with(|| {
            Arc::new(Totals {
                protocol: Mutex::new(protocol),
                retired: AtomicBool::new(false),
                listen: Mutex::new(String::new()),
                up: AtomicU64::new(0),
                down: AtomicU64::new(0),
            })
        })
        .clone()
}

/// Announces a redirect so it shows up in `stats.json` (and so in
/// `--monitor`) before its first connection.
pub fn register_redirect(name: &str, protocol: &'static str, listen: SocketAddr) {
    let _ = *STARTED_MS;
    let totals = totals_for(name, protocol);
    totals.retired.store(false, Ordering::Relaxed);
    *totals.protocol.lock().unwrap_or_else(|p| p.into_inner()) = protocol;
    *totals.listen.lock().unwrap_or_else(|p| p.into_inner()) = listen.to_string();
}

/// Retired redirects remain visible only while their old clients drain.
pub fn retire_redirect(name: &str) {
    if let Some(totals) = totals_map().get(name) {
        totals.retired.store(true, Ordering::Relaxed);
    }
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
pub struct ConnectionGuard(u64, TrafficHandle);

/// Cloneable handle for reporting a tracked connection's traffic from
/// another task (e.g. the reader of a TCP worker's byte counts).
#[derive(Clone)]
pub struct TrafficHandle {
    conn: Arc<ConnCounters>,
    totals: Arc<Totals>,
}

impl TrafficHandle {
    pub fn add_up(&self, n: u64) {
        self.conn.up.fetch_add(n, Ordering::Relaxed);
        self.totals.up.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_down(&self, n: u64) {
        self.conn.down.fetch_add(n, Ordering::Relaxed);
        self.totals.down.fetch_add(n, Ordering::Relaxed);
    }

    /// Records cumulative counts reported by a worker process. Only growth
    /// is applied, so a stale or repeated report cannot move totals back.
    pub fn set_absolute(&self, up: u64, down: u64) {
        let prev_up = self.conn.up.fetch_max(up, Ordering::Relaxed);
        self.totals
            .up
            .fetch_add(up.saturating_sub(prev_up), Ordering::Relaxed);
        let prev_down = self.conn.down.fetch_max(down, Ordering::Relaxed);
        self.totals
            .down
            .fetch_add(down.saturating_sub(prev_down), Ordering::Relaxed);
    }
}

impl ConnectionGuard {
    pub fn traffic(&self) -> TrafficHandle {
        self.1.clone()
    }
}

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
    let counters = Arc::new(ConnCounters::default());
    let handle = TrafficHandle {
        conn: counters.clone(),
        totals: totals_for(redirect, protocol),
    };
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
            counters,
        },
    );
    persist();

    ConnectionGuard(id, handle)
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
        let status_text = {
            let entries = entries();
            let shown: Vec<SocketAddr> = entries
                .values()
                .take(STATUS_LINE_CLIENT_LIMIT)
                .map(|e| e.client)
                .collect();
            format_status_text(entries.len(), &shown)
        };
        crate::notify::status(&status_text);
        write_snapshots();
    }
}

/// Builds the connection array and the per-redirect stats object from the
/// current registry.
pub fn snapshots() -> (Vec<serde_json::Value>, serde_json::Value) {
    let entries = entries();
    let connections: Vec<serde_json::Value> = entries
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
                "up_bytes": e.counters.up.load(Ordering::Relaxed),
                "down_bytes": e.counters.down.load(Ordering::Relaxed),
            })
        })
        .collect();

    let mut open: HashMap<&str, usize> = HashMap::new();
    for e in entries.values() {
        *open.entry(e.redirect.as_str()).or_default() += 1;
    }
    let mut redirects: Vec<serde_json::Value> = totals_map()
        .iter()
        .filter(|(name, t)| !t.retired.load(Ordering::Relaxed) || open.contains_key(name.as_str()))
        .map(|(name, t)| {
            serde_json::json!({
                "name": name,
                "protocol": *t.protocol.lock().unwrap_or_else(|p| p.into_inner()),
                "listen": *t.listen.lock().unwrap_or_else(|p| p.into_inner()),
                "up_total": t.up.load(Ordering::Relaxed),
                "down_total": t.down.load(Ordering::Relaxed),
                "connections": open.get(name.as_str()).copied().unwrap_or(0),
            })
        })
        .collect();
    redirects.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    (
        connections,
        serde_json::json!({ "ts_ms": now_ms(), "started_ms": *STARTED_MS, "redirects": redirects }),
    )
}

/// Writes both snapshot files. Best-effort, like `persist`.
#[cfg(unix)]
fn write_snapshots() {
    // Connection changes and the timer share fixed .tmp paths. Keep each
    // pair of writes together so neither writer can rename the other's file.
    let _write = SNAPSHOT_WRITE.lock().unwrap_or_else(|p| p.into_inner());
    let (connections, stats) = snapshots();
    let _ = write_atomic(
        STATE_FILE,
        &serde_json::to_vec(&connections).unwrap_or_default(),
    );
    let _ = write_atomic(STATS_FILE, &serde_json::to_vec(&stats).unwrap_or_default());
}

/// Refreshes the snapshots once a second so byte counters advance even
/// while no connection opens or closes. Must be called inside a tokio
/// runtime.
pub fn spawn_snapshot_task() {
    #[cfg(unix)]
    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tick.tick().await;
            write_snapshots();
        }
    });
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
fn write_atomic(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::fs;

    fs::create_dir_all(STATE_DIR)?;
    let tmp_path = format!("{path}.tmp");
    fs::write(&tmp_path, bytes)?;
    fs::rename(&tmp_path, path)?;
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
    fn reloading_protocol_updates_redirect_metadata() {
        let name = "protocol-reload-test";
        let addr = "127.0.0.1:42001".parse().unwrap();
        register_redirect(name, "tcp", addr);
        register_redirect(name, "udp", addr);
        let snapshot = snapshots().1;
        let row = snapshot["redirects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap();
        assert_eq!(row["protocol"], "udp");
    }

    #[test]
    fn retired_redirect_stays_visible_until_clients_finish() {
        let name = "retired-test";
        let addr = "127.0.0.1:42000".parse().unwrap();
        register_redirect(name, "tcp", addr);
        let guard = track(name, "tcp", addr, addr);
        retire_redirect(name);
        let visible = || {
            snapshots().1["redirects"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["name"] == name)
        };
        assert!(visible());
        drop(guard);
        assert!(!visible());
        register_redirect(name, "tcp", addr);
        assert!(visible());
    }

    #[test]
    fn track_and_drop_roundtrip_updates_registry() {
        let client: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let target: SocketAddr = "127.0.0.1:40001".parse().unwrap();

        let guard = track("test-redirect", "tcp", client, target);
        let id = guard.0;
        assert!(is_registered(id), "tracked connection should be registered");

        drop(guard);
        assert!(
            !is_registered(id),
            "dropping the guard should deregister it"
        );
    }

    #[test]
    fn traffic_is_reported_per_connection_and_per_redirect() {
        let client: SocketAddr = "127.0.0.1:41000".parse().unwrap();
        let target: SocketAddr = "127.0.0.1:41001".parse().unwrap();
        let guard = track("traffic-test", "tcp", client, target);
        let handle = guard.traffic();
        handle.add_up(10);
        handle.add_down(5);
        handle.set_absolute(30, 5);
        handle.set_absolute(20, 4); // stale report: ignored
        drop(handle);

        let (connections, stats) = snapshots();
        let conn = connections
            .iter()
            .find(|c| c["redirect"] == "traffic-test")
            .unwrap();
        assert_eq!(conn["up_bytes"], 30);
        assert_eq!(conn["down_bytes"], 5);

        drop(guard);
        let (_, stats_after) = snapshots();
        let find = |v: &serde_json::Value| {
            v["redirects"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["name"] == "traffic-test")
                .unwrap()
                .clone()
        };
        assert_eq!(find(&stats)["connections"], 1);
        let after = find(&stats_after);
        assert_eq!(after["connections"], 0);
        assert_eq!(after["up_total"], 30, "totals outlive the connection");
        assert_eq!(after["down_total"], 5);
    }
}
