//! Exercises `proxy::run` end-to-end over real loopback sockets: a fake
//! "backend" TCP server plus a real client, with the redir-rust proxy
//! sitting in between exactly as it would in production (just without a
//! config file or systemd around it).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use redir_rust::plugin::{FailureAction, FailureContext, Plugin};
use redir_rust::proxy::{self, ProxyConfig};

struct ProxyHarness {
    addr: SocketAddr,
    task: JoinHandle<std::io::Result<()>>,
}

impl Drop for ProxyHarness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_proxy(
    name: &str,
    targets: Vec<SocketAddr>,
    connect_timeout: Duration,
    plugins: Vec<Arc<dyn Plugin>>,
) -> ProxyHarness {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ProxyConfig::new(
        name,
        "127.0.0.1:0".parse().unwrap(),
        targets,
        connect_timeout,
        None,
    )
    .with_worker_executable(env!("CARGO_BIN_EXE_redir-rust"));
    let task = tokio::spawn(proxy::run_with_listener(config, listener, plugins));
    ProxyHarness { addr, task }
}

#[derive(Default)]
struct RecordingPlugin {
    failure_calls: AtomicUsize,
    failure_context: Mutex<Option<FailureContext>>,
    applies_to_addr: Mutex<Option<SocketAddr>>,
}

#[async_trait]
impl Plugin for RecordingPlugin {
    fn name(&self) -> &str {
        "recording"
    }

    fn applies_to(&self, listen_addr: SocketAddr) -> bool {
        *self.applies_to_addr.lock().unwrap() = Some(listen_addr);
        true
    }

    async fn on_target_failure(
        &self,
        ctx: &FailureContext,
        _client: &mut TcpStream,
    ) -> FailureAction {
        self.failure_calls.fetch_add(1, Ordering::SeqCst);
        *self.failure_context.lock().unwrap() = Some(ctx.clone());
        FailureAction::PassThrough
    }
}

/// Spawns a fake backend that echoes back each connection's data,
/// uppercased (so a wrong wiring -- e.g. a client looped back to itself --
/// would show up as a case mismatch, not a false pass).
async fn spawn_echo_backend() -> SocketAddr {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = backend.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return; // e.g. a readiness probe that sent no data
                }
                let mut reply = buf[..n].to_vec();
                reply.make_ascii_uppercase();
                let _ = sock.write_all(&reply).await;
            });
        }
    });
    addr
}

async fn spawn_tagged_backend(tag: u8) -> SocketAddr {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = backend.local_addr().unwrap();
    spawn_tagged_backend_on(backend, tag);
    addr
}

fn spawn_tagged_backend_on(backend: TcpListener, tag: u8) {
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = backend.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    if sock.write_all(&[tag]).await.is_err()
                        || sock.write_all(&buf[..n]).await.is_err()
                    {
                        break;
                    }
                }
            });
        }
    });
}

async fn assert_tagged_reply(client: &mut TcpStream, tag: u8, payload: &[u8]) {
    client.write_all(payload).await.unwrap();
    let mut reply = vec![0; payload.len() + 1];
    timeout(Duration::from_secs(2), client.read_exact(&mut reply))
        .await
        .expect("read timed out")
        .unwrap();
    assert_eq!(reply[0], tag);
    assert_eq!(&reply[1..], payload);
}

#[tokio::test]
async fn relays_data_bidirectionally() {
    let backend_addr = spawn_echo_backend().await;
    let proxy = spawn_proxy(
        "test",
        vec![backend_addr],
        Duration::from_secs(2),
        Vec::new(),
    )
    .await;

    let mut client = TcpStream::connect(proxy.addr).await.unwrap();
    client.write_all(b"hello").await.unwrap();

    let mut buf = [0u8; 1024];
    let n = timeout(Duration::from_secs(2), client.read(&mut buf))
        .await
        .expect("read timed out")
        .unwrap();
    assert_eq!(&buf[..n], b"HELLO");
}

#[tokio::test]
async fn all_targets_unreachable_calls_failure_hook_once_with_final_context_and_closes_client() {
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let targets = vec![first.local_addr().unwrap(), second.local_addr().unwrap()];
    let final_target = targets[1];
    let plugin = Arc::new(RecordingPlugin::default());
    let proxy = spawn_proxy(
        "test-unreachable",
        targets,
        Duration::from_millis(500),
        vec![plugin.clone()],
    )
    .await;
    drop((first, second));

    let mut client = TcpStream::connect(proxy.addr).await.unwrap();
    let client_addr = client.local_addr().unwrap();
    let mut buf = [0u8; 16];
    let n = timeout(Duration::from_secs(2), client.read(&mut buf))
        .await
        .expect("read timed out")
        .unwrap();
    assert_eq!(
        n, 0,
        "expected EOF (proxy closes the client when the target is unreachable)"
    );
    assert_eq!(plugin.failure_calls.load(Ordering::SeqCst), 1);
    let ctx = plugin.failure_context.lock().unwrap().clone().unwrap();
    assert_eq!(ctx.client_addr, client_addr);
    assert_eq!(ctx.listen_addr, proxy.addr);
    assert_eq!(ctx.target_addr, final_target);
    assert!(!ctx.error.is_empty());
    assert_eq!(*plugin.applies_to_addr.lock().unwrap(), Some(proxy.addr));
}

#[tokio::test]
async fn selects_primary_when_both_targets_are_reachable() {
    let primary = spawn_tagged_backend(b'P').await;
    let secondary = spawn_tagged_backend(b'S').await;
    let proxy = spawn_proxy(
        "test-primary",
        vec![primary, secondary],
        Duration::from_secs(2),
        Vec::new(),
    )
    .await;

    let mut client = TcpStream::connect(proxy.addr).await.unwrap();
    assert_tagged_reply(&mut client, b'P', b"primary").await;
}

#[tokio::test]
async fn successful_fallback_does_not_call_failure_hook() {
    let secondary = spawn_tagged_backend(b'S').await;
    let primary_reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let primary = primary_reservation.local_addr().unwrap();
    let plugin = Arc::new(RecordingPlugin::default());
    let proxy = spawn_proxy(
        "test-secondary",
        vec![primary, secondary],
        Duration::from_millis(200),
        vec![plugin.clone()],
    )
    .await;
    drop(primary_reservation);

    let mut client = TcpStream::connect(proxy.addr).await.unwrap();
    assert_tagged_reply(&mut client, b'S', b"secondary").await;
    assert_eq!(plugin.failure_calls.load(Ordering::SeqCst), 0);
    assert!(plugin.failure_context.lock().unwrap().is_none());
}

#[tokio::test]
async fn existing_connection_stays_pinned_while_new_connection_fails_back() {
    let secondary = spawn_tagged_backend(b'S').await;
    let primary_reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let primary_addr = primary_reservation.local_addr().unwrap();
    let proxy = spawn_proxy(
        "test-failback",
        vec![primary_addr, secondary],
        Duration::from_millis(200),
        Vec::new(),
    )
    .await;
    drop(primary_reservation);

    let mut client_a = TcpStream::connect(proxy.addr).await.unwrap();
    assert_tagged_reply(&mut client_a, b'S', b"before").await;

    let primary = TcpListener::bind(primary_addr).await.unwrap();
    spawn_tagged_backend_on(primary, b'P');

    assert_tagged_reply(&mut client_a, b'S', b"after").await;
    let mut client_b = TcpStream::connect(proxy.addr).await.unwrap();
    assert_tagged_reply(&mut client_b, b'P', b"new").await;
}
