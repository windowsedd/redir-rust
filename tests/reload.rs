//! Black-box reload coverage using the executable and real loopback sockets.
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Running {
    child: Child,
    dir: PathBuf,
    config: PathBuf,
}

impl Running {
    fn start(text: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "redir-reload-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        fs::write(&config, text).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_redir-rust"))
            .args(["--config", config.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self { child, dir, config }
    }

    fn reload(&mut self, text: &str) -> std::process::Output {
        fs::write(&self.config, text).unwrap();
        Command::new(env!("CARGO_BIN_EXE_redir-rust"))
            .args(["--reload", "--config", self.config.to_str().unwrap()])
            .output()
            .unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        fs::remove_dir_all(&self.dir).ok();
    }
}

fn free_tcp() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn connect(addr: SocketAddr) -> TcpStream {
    let until = Instant::now() + Duration::from_secs(3);
    let stream = loop {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(100)) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => panic!("proxy did not bind {addr}: {err}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
}

fn tcp_backend(tag: u8) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        // Finite accept loop so a failed test doesn't retain a server forever.
        listener.set_nonblocking(true).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            match listener.accept() {
                Ok((mut client, _)) => {
                    std::thread::spawn(move || {
                        client
                            .set_read_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        let mut byte = [0];
                        while client.read_exact(&mut byte).is_ok() {
                            if client.write_all(&[tag]).is_err() {
                                break;
                            }
                        }
                    });
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    });
    addr
}

fn exchange(client: &mut TcpStream, expected: u8) {
    client.write_all(b"x").unwrap();
    let mut response = [0];
    client.read_exact(&mut response).unwrap();
    assert_eq!(response[0], expected);
}

fn tcp_config(listen: SocketAddr, backend: SocketAddr) -> String {
    format!("[[redirect]]\nname='tcp'\nlisten='{listen}'\ntarget='{backend}'\n")
}

fn assert_ok(output: std::process::Output) {
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn tcp_reload_keeps_old_connections_and_rolls_back_bad_updates() {
    let primary = tcp_backend(b'A');
    let replacement = tcp_backend(b'B');
    let listen = free_tcp();
    let original = tcp_config(listen, primary);
    let mut running = Running::start(&original);
    let mut old = connect(listen);
    exchange(&mut old, b'A');
    assert_ok(running.reload(&tcp_config(listen, replacement)));
    exchange(&mut old, b'A');
    exchange(&mut connect(listen), b'B');
    let extra_listen = free_tcp();
    let extra = tcp_config(extra_listen, primary).replace("name='tcp'", "name='extra'");
    assert_ok(running.reload(&(tcp_config(listen, replacement) + &extra)));
    exchange(&mut connect(extra_listen), b'A');
    assert!(!running
        .reload(&(original.clone() + &original.replace("name='tcp'", "name='duplicate'")))
        .status
        .success());
    exchange(&mut connect(listen), b'B');
    let missing_icon = format!(
        "{}[redirect.minecraft.plugins]\nenabled=true\nfavicon_path='missing-favicon-{}.png'\n",
        original,
        std::process::id()
    );
    assert!(!running.reload(&missing_icon).status.success());
    assert!(!running.reload("this is not TOML").status.success());
    exchange(&mut connect(listen), b'B');
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let conflict =
        tcp_config(occupied.local_addr().unwrap(), primary).replace("name='tcp'", "name='extra'");
    let temporary_listen = free_tcp();
    let temporary = tcp_config(temporary_listen, primary).replace("name='tcp'", "name='temporary'");
    assert!(!running
        .reload(&(original.clone() + &temporary + &conflict))
        .status
        .success());
    assert!(
        TcpListener::bind(temporary_listen).is_ok(),
        "failed transaction leaked its prepared socket"
    );
    exchange(&mut old, b'A');
    exchange(&mut connect(listen), b'B');
    assert_ok(running.reload("# all redirects removed\n"));
    exchange(&mut old, b'A');
    let until = Instant::now() + Duration::from_secs(2);
    while TcpStream::connect(listen).is_ok() {
        assert!(Instant::now() < until, "removed TCP listener still accepts");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_ok(running.reload(&original));
    exchange(&mut connect(listen), b'A');
}

fn udp_backend(tag: u8) -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    std::thread::spawn(move || {
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buf = [0u8; 64];
        while let Ok((_, client)) = socket.recv_from(&mut buf) {
            if socket.send_to(&[tag], client).is_err() {
                break;
            }
        }
    });
    addr
}

fn udp_client() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
}

fn udp_exchange(client: &UdpSocket, listen: SocketAddr, expected: u8) {
    client.send_to(b"x", listen).unwrap();
    let mut reply = [0];
    client.recv(&mut reply).unwrap();
    assert_eq!(reply[0], expected);
}

#[test]
fn udp_reload_pins_existing_sessions_and_drains_removed_listener() {
    let primary = udp_backend(b'A');
    let replacement = udp_backend(b'B');
    let listen = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let anchor = free_tcp();
    let config = |target| {
        format!("{}\n[[redirect]]\nname='udp'\nlisten='{listen}'\ntarget='{target}'\nprotocol='udp'\nudp_idle_timeout_ms=1000\n", tcp_config(anchor, "127.0.0.1:9".parse().unwrap()))
    };
    let mut running = Running::start(&config(primary));
    // The TCP listener is bound in the same startup transaction.
    drop(connect(anchor));
    let old = udp_client();
    udp_exchange(&old, listen, b'A');
    assert_ok(running.reload(&config(replacement)));
    udp_exchange(&old, listen, b'A');
    let new = udp_client();
    udp_exchange(&new, listen, b'B');
    assert_ok(running.reload(&tcp_config(anchor, "127.0.0.1:9".parse().unwrap())));
    udp_exchange(&old, listen, b'A');
    udp_exchange(&new, listen, b'B');
    let newcomer = udp_client();
    newcomer
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    newcomer.send_to(b"x", listen).unwrap();
    assert!(
        newcomer.recv(&mut [0]).is_err(),
        "removed listener admitted a new UDP client"
    );
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        if UdpSocket::bind(listen).is_ok() {
            break;
        }
        assert!(
            Instant::now() < until,
            "drained UDP listener did not release its port"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn endpoint(running: &Running) -> serde_json::Value {
    let path = running.config.with_file_name("config.toml.reload.json");
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(text) = fs::read_to_string(&path) {
            return serde_json::from_str(&text).unwrap();
        }
        assert!(
            Instant::now() < until,
            "reload control endpoint not created"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn reload_control_rejects_bad_tokens_and_oversized_requests() {
    let primary = tcp_backend(b'A');
    let replacement = tcp_backend(b'B');
    let listen = free_tcp();
    let mut running = Running::start(&tcp_config(listen, primary));
    let metadata = endpoint(&running);
    let control: SocketAddr = metadata["address"].as_str().unwrap().parse().unwrap();
    assert_eq!(control.ip(), std::net::Ipv4Addr::LOCALHOST);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = running.config.with_file_name("config.toml.reload.json");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    fs::write(&running.config, tcp_config(listen, replacement)).unwrap();
    for request in ["invalid-token reload\n".to_string(), "x".repeat(1024)] {
        let mut stream = connect(control);
        stream.write_all(request.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = String::new();
        // Oversized requests can reset a connection after the bounded read.
        let _ = stream.read_to_string(&mut response);
        assert!(response.starts_with("ERROR"), "{response}");
        exchange(&mut connect(listen), b'A');
    }
    // Silent, unauthenticated connections must not serialize their two-second
    // handshake timeouts ahead of an authorized reload.
    let silent: Vec<_> = (0..8).map(|_| connect(control)).collect();
    assert_ok(running.reload(&tcp_config(listen, replacement)));
    drop(silent);
    exchange(&mut connect(listen), b'B');
    // A force-killed process leaves metadata behind. Restart must replace it.
    let old_token = metadata["token"].clone();
    running.child.kill().unwrap();
    running.child.wait().unwrap();
    running.child = Command::new(env!("CARGO_BIN_EXE_redir-rust"))
        .args(["--config", running.config.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while endpoint(&running)["token"] == old_token {
        assert!(
            Instant::now() < until,
            "stale control metadata not replaced"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_ok(running.reload(&tcp_config(listen, primary)));
    exchange(&mut connect(listen), b'A');
}

#[cfg(unix)]
#[test]
fn sighup_reloads_config_without_dropping_established_connection() {
    let primary = tcp_backend(b'A');
    let replacement = tcp_backend(b'B');
    let listen = free_tcp();
    let running = Running::start(&tcp_config(listen, primary));
    let mut old = connect(listen);
    exchange(&mut old, b'A');
    fs::write(&running.config, tcp_config(listen, replacement)).unwrap();
    // SAFETY: send SIGHUP only to the live child owned by this test.
    assert_eq!(
        unsafe { libc::kill(running.child.id() as i32, libc::SIGHUP) },
        0
    );
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let mut new = connect(listen);
        new.write_all(b"x").unwrap();
        let mut response = [0];
        new.read_exact(&mut response).unwrap();
        if response[0] == b'B' {
            break;
        }
        assert!(Instant::now() < until, "SIGHUP did not apply new target");
        std::thread::sleep(Duration::from_millis(10));
    }
    exchange(&mut old, b'A');
}
