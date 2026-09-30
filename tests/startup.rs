#[cfg(unix)]
#[test]
fn failed_listener_exits_without_reporting_ready() {
    use std::io::ErrorKind;
    use std::net::TcpListener;
    use std::os::unix::net::UnixDatagram;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();
    let base = std::env::temp_dir().join(format!("redir-startup-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let config_path = base.join("config.toml");
    let notify_path = base.join("notify.sock");
    std::fs::write(&config_path, format!("[[redirect]]\nlisten = '127.0.0.1:0'\ntarget = '127.0.0.1:9'\n[[redirect]]\nlisten = '127.0.0.1:{port}'\ntarget = '127.0.0.1:9'\n")).unwrap();
    let notify = UnixDatagram::bind(&notify_path).unwrap();
    notify.set_nonblocking(true).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_redir-rust"))
        .args(["--config", config_path.to_str().unwrap()])
        .env("NOTIFY_SOCKET", &notify_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            panic!("service stayed alive after a listener failed");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    let mut message = [0u8; 256];
    assert!(matches!(notify.recv(&mut message), Err(err) if err.kind() == ErrorKind::WouldBlock));
    std::fs::remove_dir_all(base).ok();
}
