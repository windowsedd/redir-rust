//! Minimal `sd_notify(3)` client: lets the process report readiness/status
//! to systemd (`Type=notify` units) directly in `systemctl status`'s
//! `Status:` line, without a `libsystemd` dependency -- just a single
//! `UnixDatagram` send to `$NOTIFY_SOCKET`. No-op (safe to call
//! unconditionally) when not running under systemd, e.g. manual/dev runs,
//! non-Linux, or when `$NOTIFY_SOCKET` points to an abstract-namespace
//! socket (unhandled here; system services almost always use a plain
//! filesystem path).

#[cfg(target_os = "linux")]
pub fn ready() {
    send("READY=1");
}

#[cfg(target_os = "linux")]
pub fn status(text: &str) {
    send(&format!("STATUS={text}"));
}

#[cfg(not(target_os = "linux"))]
pub fn ready() {}

#[cfg(not(target_os = "linux"))]
pub fn status(_text: &str) {}

#[cfg(target_os = "linux")]
fn send(message: &str) {
    use std::os::unix::net::UnixDatagram;

    let Ok(socket_path) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    if socket_path.is_empty() || socket_path.starts_with('@') {
        return;
    }

    let Ok(socket) = UnixDatagram::unbound() else {
        return;
    };
    let _ = socket.send_to(message.as_bytes(), &socket_path);
}
