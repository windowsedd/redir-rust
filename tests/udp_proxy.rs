//! End-to-end UDP relay tests using real loopback sockets. Bedrock-aware
//! backends answer RakNet Unconnected Ping probes and tag application data.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use redir_rust::plugins::bedrock_offline::{self, BedrockOfflineConfig};
use redir_rust::udp_proxy::{self, UdpProxyConfig};

const TEST_PROBE_TIMEOUT: Duration = Duration::from_millis(75);
const RAKNET_MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];

struct TestProxy {
    listen_addr: SocketAddr,
    task: JoinHandle<std::io::Result<()>>,
}

impl TestProxy {
    fn addr(&self) -> SocketAddr {
        assert!(
            !self.task.is_finished(),
            "udp proxy loop exited unexpectedly"
        );
        self.listen_addr
    }
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_echo_backend() -> SocketAddr {
    let backend = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        loop {
            let Ok((n, from)) = backend.recv_from(&mut buf).await else {
                break;
            };
            let _ = backend.send_to(&buf[..n], from).await;
        }
    });
    addr
}

async fn spawn_bedrock_backend(
    tag: &'static [u8],
    reachable: Arc<AtomicBool>,
    probe_delay: Duration,
) -> SocketAddr {
    spawn_bedrock_backend_on("127.0.0.1:0", tag, reachable, probe_delay)
        .await
        .unwrap()
}

async fn spawn_bedrock_backend_on(
    bind_addr: &str,
    tag: &'static [u8],
    reachable: Arc<AtomicBool>,
    probe_delay: Duration,
) -> std::io::Result<SocketAddr> {
    let backend = UdpSocket::bind(bind_addr).await?;
    let addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        let pong_config = BedrockOfflineConfig {
            motd_line1: "backend".to_string(),
            motd_line2: "ready".to_string(),
            server_guid: 7,
        };
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, from)) = backend.recv_from(&mut buf).await else {
                break;
            };
            if bedrock_offline::is_unconnected_ping(&buf[..n]) {
                if reachable.load(Ordering::Relaxed) {
                    tokio::time::sleep(probe_delay).await;
                    let pong = bedrock_offline::build_unconnected_pong(
                        &buf[..n],
                        &pong_config,
                        addr.port(),
                    )
                    .unwrap();
                    let _ = backend.send_to(&pong, from).await;
                }
            } else {
                let mut reply = tag.to_vec();
                reply.extend_from_slice(&buf[..n]);
                let _ = backend.send_to(&reply, from).await;
            }
        }
    });
    Ok(addr)
}

async fn spawn_malformed_pong_backend(tag: &'static [u8]) -> SocketAddr {
    let backend = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, from)) = backend.recv_from(&mut buf).await else {
                break;
            };
            if bedrock_offline::is_unconnected_ping(&buf[..n]) {
                let _ = backend.send_to(&[0x1c], from).await;
            } else {
                let mut reply = tag.to_vec();
                reply.extend_from_slice(&buf[..n]);
                let _ = backend.send_to(&reply, from).await;
            }
        }
    });
    addr
}

async fn spawn_truncated_pong_backend(tag: &'static [u8]) -> SocketAddr {
    let backend = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, from)) = backend.recv_from(&mut buf).await else {
                break;
            };
            if bedrock_offline::is_unconnected_ping(&buf[..n]) {
                let mut pong = vec![0x1c];
                pong.extend_from_slice(&buf[1..9]);
                pong.extend_from_slice(&7i64.to_be_bytes());
                pong.extend_from_slice(&buf[9..25]);
                pong.extend_from_slice(&1u16.to_be_bytes());
                let _ = backend.send_to(&pong, from).await;
            } else {
                let mut reply = tag.to_vec();
                reply.extend_from_slice(&buf[..n]);
                let _ = backend.send_to(&reply, from).await;
            }
        }
    });
    addr
}

async fn spawn_large_valid_pong_backend(tag: &'static [u8]) -> SocketAddr {
    let backend = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, from)) = backend.recv_from(&mut buf).await else {
                break;
            };
            if bedrock_offline::is_unconnected_ping(&buf[..n]) {
                let payload = vec![b'x'; 2_000];
                let mut pong = vec![0x1c];
                pong.extend_from_slice(&buf[1..9]);
                pong.extend_from_slice(&7i64.to_be_bytes());
                pong.extend_from_slice(&buf[9..25]);
                pong.extend_from_slice(&(payload.len() as u16).to_be_bytes());
                pong.extend_from_slice(&payload);
                let _ = backend.send_to(&pong, from).await;
            } else {
                let mut reply = tag.to_vec();
                reply.extend_from_slice(&buf[..n]);
                let _ = backend.send_to(&reply, from).await;
            }
        }
    });
    addr
}

async fn spawn_proxy(
    targets: Vec<SocketAddr>,
    bedrock_offline: Option<BedrockOfflineConfig>,
) -> TestProxy {
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let listen_addr = listener.local_addr().unwrap();
    let config = UdpProxyConfig {
        name: "test-udp".to_string(),
        listen_addr,
        targets,
        idle_timeout: Duration::from_secs(5),
        probe_timeout: TEST_PROBE_TIMEOUT,
        bedrock_offline,
    };
    let task = tokio::spawn(udp_proxy::run_with_socket(config, listener));
    tokio::task::yield_now().await;
    let proxy = TestProxy { listen_addr, task };
    let _ = proxy.addr();
    proxy
}

async fn recv(client: &UdpSocket) -> Vec<u8> {
    let mut buf = [0u8; 2048];
    let (n, _) = timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("recv timed out")
        .unwrap();
    buf[..n].to_vec()
}

fn unconnected_ping() -> Vec<u8> {
    let mut ping = vec![0x01];
    ping.extend_from_slice(&123i64.to_be_bytes());
    ping.extend_from_slice(&RAKNET_MAGIC);
    ping.extend_from_slice(&999i64.to_be_bytes());
    ping
}

#[tokio::test]
async fn single_target_generic_relay_does_not_require_bedrock_probe() {
    let backend_addr = spawn_echo_backend().await;
    let proxy = spawn_proxy(vec![backend_addr], None).await;
    let listen_addr = proxy.addr();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"hello", listen_addr).await.unwrap();
    assert_eq!(recv(&client).await, b"hello");
}

#[tokio::test]
async fn separate_clients_get_independent_sessions() {
    let backend_addr = spawn_echo_backend().await;
    let proxy = spawn_proxy(vec![backend_addr], None).await;
    let listen_addr = proxy.addr();
    let client_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let client_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client_a.send_to(b"from-a", listen_addr).await.unwrap();
    client_b.send_to(b"from-b", listen_addr).await.unwrap();
    assert_eq!(recv(&client_a).await, b"from-a");
    assert_eq!(recv(&client_b).await, b"from-b");
}

#[tokio::test]
async fn multi_target_selects_primary_when_both_respond() {
    let primary =
        spawn_bedrock_backend(b"primary:", Arc::new(AtomicBool::new(true)), Duration::ZERO).await;
    let secondary = spawn_bedrock_backend(
        b"secondary:",
        Arc::new(AtomicBool::new(true)),
        Duration::ZERO,
    )
    .await;
    let proxy = spawn_proxy(vec![primary, secondary], None).await;
    let listen_addr = proxy.addr();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"one", listen_addr).await.unwrap();
    assert_eq!(recv(&client).await, b"primary:one");
}

#[tokio::test]
async fn multi_target_selects_secondary_when_primary_is_silent() {
    let primary = spawn_bedrock_backend(
        b"primary:",
        Arc::new(AtomicBool::new(false)),
        Duration::ZERO,
    )
    .await;
    let secondary = spawn_bedrock_backend(
        b"secondary:",
        Arc::new(AtomicBool::new(true)),
        Duration::ZERO,
    )
    .await;
    let proxy = spawn_proxy(vec![primary, secondary], None).await;
    let listen_addr = proxy.addr();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"one", listen_addr).await.unwrap();
    assert_eq!(recv(&client).await, b"secondary:one");
}

#[tokio::test]
async fn ipv6_multi_target_selects_primary_and_relays_data() {
    let primary = match spawn_bedrock_backend_on(
        "[::1]:0",
        b"ipv6-primary:",
        Arc::new(AtomicBool::new(true)),
        Duration::ZERO,
    )
    .await
    {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("skipping IPv6 UDP test because [::1] is unavailable: {err}");
            return;
        }
    };
    let secondary = spawn_bedrock_backend_on(
        "[::1]:0",
        b"ipv6-secondary:",
        Arc::new(AtomicBool::new(true)),
        Duration::ZERO,
    )
    .await
    .unwrap();
    let proxy = spawn_proxy(vec![primary, secondary], None).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    client.send_to(b"hello-v6", proxy.addr()).await.unwrap();

    assert_eq!(recv(&client).await, b"ipv6-primary:hello-v6");
}

#[tokio::test]
async fn mixed_ipv6_ipv4_targets_preserve_priority_and_relay_data() {
    let primary = match spawn_bedrock_backend_on(
        "[::1]:0",
        b"ipv6-primary:",
        Arc::new(AtomicBool::new(true)),
        Duration::ZERO,
    )
    .await
    {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("skipping IPv6 UDP test because [::1] is unavailable: {err}");
            return;
        }
    };
    let secondary = spawn_bedrock_backend(
        b"ipv4-secondary:",
        Arc::new(AtomicBool::new(true)),
        Duration::ZERO,
    )
    .await;
    let proxy = spawn_proxy(vec![primary, secondary], None).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    client.send_to(b"mixed", proxy.addr()).await.unwrap();

    assert_eq!(recv(&client).await, b"ipv6-primary:mixed");
}

#[tokio::test]
async fn multi_target_rejects_malformed_primary_pong() {
    let primary = spawn_malformed_pong_backend(b"malformed-primary:").await;
    let secondary = spawn_bedrock_backend(
        b"secondary:",
        Arc::new(AtomicBool::new(true)),
        Duration::ZERO,
    )
    .await;
    let proxy = spawn_proxy(vec![primary, secondary], None).await;
    let listen_addr = proxy.addr();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"one", listen_addr).await.unwrap();
    assert_eq!(recv(&client).await, b"secondary:one");
}

#[tokio::test]
async fn multi_target_rejects_truncated_primary_pong_payload() {
    let primary = spawn_truncated_pong_backend(b"truncated-primary:").await;
    let secondary = spawn_bedrock_backend(
        b"secondary:",
        Arc::new(AtomicBool::new(true)),
        Duration::ZERO,
    )
    .await;
    let proxy = spawn_proxy(vec![primary, secondary], None).await;
    let listen_addr = proxy.addr();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"one", listen_addr).await.unwrap();
    assert_eq!(recv(&client).await, b"secondary:one");
}

#[tokio::test]
async fn multi_target_accepts_large_valid_primary_pong() {
    let primary = spawn_large_valid_pong_backend(b"large-primary:").await;
    let secondary = spawn_bedrock_backend(
        b"secondary:",
        Arc::new(AtomicBool::new(true)),
        Duration::ZERO,
    )
    .await;
    let proxy = spawn_proxy(vec![primary, secondary], None).await;
    let listen_addr = proxy.addr();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"one", listen_addr).await.unwrap();
    assert_eq!(recv(&client).await, b"large-primary:one");
}

#[tokio::test]
async fn datagrams_queued_during_selection_are_forwarded_once_in_order() {
    let primary = spawn_bedrock_backend(
        b"primary:",
        Arc::new(AtomicBool::new(true)),
        Duration::from_millis(40),
    )
    .await;
    let secondary = spawn_bedrock_backend(
        b"secondary:",
        Arc::new(AtomicBool::new(true)),
        Duration::ZERO,
    )
    .await;
    let proxy = spawn_proxy(vec![primary, secondary], None).await;
    let listen_addr = proxy.addr();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"first", listen_addr).await.unwrap();
    client.send_to(b"second", listen_addr).await.unwrap();
    assert_eq!(recv(&client).await, b"primary:first");
    assert_eq!(recv(&client).await, b"primary:second");
    let mut extra = [0u8; 32];
    assert!(
        timeout(Duration::from_millis(100), client.recv_from(&mut extra))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn established_session_stays_pinned_while_new_session_fails_back() {
    let primary_reachable = Arc::new(AtomicBool::new(false));
    let primary =
        spawn_bedrock_backend(b"primary:", primary_reachable.clone(), Duration::ZERO).await;
    let secondary = spawn_bedrock_backend(
        b"secondary:",
        Arc::new(AtomicBool::new(true)),
        Duration::ZERO,
    )
    .await;
    let proxy = spawn_proxy(vec![primary, secondary], None).await;
    let listen_addr = proxy.addr();
    let client_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client_a.send_to(b"a1", listen_addr).await.unwrap();
    assert_eq!(recv(&client_a).await, b"secondary:a1");
    primary_reachable.store(true, Ordering::Relaxed);
    client_a.send_to(b"a2", listen_addr).await.unwrap();
    assert_eq!(recv(&client_a).await, b"secondary:a2");
    let client_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client_b.send_to(b"b1", listen_addr).await.unwrap();
    assert_eq!(recv(&client_b).await, b"primary:b1");
}

#[tokio::test]
async fn all_targets_silent_with_offline_enabled_returns_configured_pong() {
    let primary = spawn_bedrock_backend(
        b"primary:",
        Arc::new(AtomicBool::new(false)),
        Duration::ZERO,
    )
    .await;
    let secondary = spawn_bedrock_backend(
        b"secondary:",
        Arc::new(AtomicBool::new(false)),
        Duration::ZERO,
    )
    .await;
    let offline = BedrockOfflineConfig {
        motd_line1: "Planned maintenance".to_string(),
        motd_line2: "Try later".to_string(),
        server_guid: 42,
    };
    let proxy = spawn_proxy(vec![primary, secondary], Some(offline)).await;
    let listen_addr = proxy.addr();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(&unconnected_ping(), listen_addr)
        .await
        .unwrap();
    let pong = recv(&client).await;
    assert_eq!(pong[0], 0x1c);
    let motd = String::from_utf8(pong[35..].to_vec()).unwrap();
    assert!(motd.contains("Planned maintenance"));
    assert!(motd.contains("Try later"));
}

#[tokio::test]
async fn expired_fake_session_reprobes_recovered_primary() {
    let primary_reachable = Arc::new(AtomicBool::new(false));
    let primary =
        spawn_bedrock_backend(b"primary:", primary_reachable.clone(), Duration::ZERO).await;
    let secondary = spawn_bedrock_backend(
        b"secondary:",
        Arc::new(AtomicBool::new(false)),
        Duration::ZERO,
    )
    .await;
    let offline = BedrockOfflineConfig {
        motd_line1: "Offline".to_string(),
        motd_line2: "Retry".to_string(),
        server_guid: 42,
    };
    let proxy = spawn_proxy(vec![primary, secondary], Some(offline)).await;
    let listen_addr = proxy.addr();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    client.send_to(&[0x05], listen_addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    tokio::time::sleep(bedrock_offline::FAKE_SESSION_IDLE_TIMEOUT + Duration::from_millis(10))
        .await;
    primary_reachable.store(true, Ordering::Relaxed);
    client.send_to(b"recovered", listen_addr).await.unwrap();

    assert_eq!(recv(&client).await, b"primary:recovered");
}

#[tokio::test]
async fn all_targets_silent_without_offline_drops_and_listener_continues() {
    let primary_reachable = Arc::new(AtomicBool::new(false));
    let primary =
        spawn_bedrock_backend(b"primary:", primary_reachable.clone(), Duration::ZERO).await;
    let secondary = spawn_bedrock_backend(
        b"secondary:",
        Arc::new(AtomicBool::new(false)),
        Duration::ZERO,
    )
    .await;
    let proxy = spawn_proxy(vec![primary, secondary], None).await;
    let listen_addr = proxy.addr();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut buf = [0u8; 32];
    client.send_to(b"first", listen_addr).await.unwrap();
    assert!(
        timeout(Duration::from_millis(250), client.recv_from(&mut buf))
            .await
            .is_err()
    );
    primary_reachable.store(true, Ordering::Relaxed);
    client.send_to(b"second", listen_addr).await.unwrap();
    assert_eq!(recv(&client).await, b"primary:second");
}

#[tokio::test]
async fn empty_target_list_is_rejected_before_binding() {
    let occupied_listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let listen_addr = occupied_listener.local_addr().unwrap();
    let error = udp_proxy::run(UdpProxyConfig {
        name: "empty".to_string(),
        listen_addr,
        targets: Vec::new(),
        idle_timeout: Duration::from_secs(1),
        probe_timeout: TEST_PROBE_TIMEOUT,
        bedrock_offline: None,
    })
    .await
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}
