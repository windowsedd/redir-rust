use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::connections;
use crate::plugins::bedrock_offline::{
    self, BedrockOfflineConfig, FakeSession, FAKE_SESSION_IDLE_TIMEOUT,
};

const MAX_DATAGRAM: usize = 65_527;
/// Limits memory retained while a client's ordered target probes are in flight.
const MAX_PENDING_DATAGRAMS: usize = 64;
const MAX_PENDING_CLIENTS: usize = 1_024;
const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;
const MAX_FAKE_SESSIONS: usize = 1_024;
const PROBE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct UdpProxyConfig {
    pub name: String,
    pub listen_addr: SocketAddr,
    pub targets: Vec<SocketAddr>,
    pub idle_timeout: Duration,
    pub probe_timeout: Duration,
    /// When set, Unconnected Ping packets get a synthesized offline Pong
    /// instead of being forwarded whenever every target is unreachable.
    pub bedrock_offline: Option<BedrockOfflineConfig>,
}

struct Session {
    socket: Arc<UdpSocket>,
    last_seen: Arc<StdMutex<Instant>>,
    _tracking: connections::ConnectionGuard,
}

type Sessions = Arc<Mutex<HashMap<SocketAddr, Session>>>;

struct PendingSelection {
    generation: u64,
    datagrams: VecDeque<Vec<u8>>,
    overflow_logged: bool,
}

struct PendingState {
    selections: HashMap<SocketAddr, PendingSelection>,
    total_bytes: usize,
    max_clients: usize,
    max_bytes: usize,
}

enum EnqueueResult {
    Queued,
    PerClientFull { first_drop: bool },
    GloballyFull,
}

impl PendingState {
    fn new(max_clients: usize, max_bytes: usize) -> Self {
        Self {
            selections: HashMap::new(),
            total_bytes: 0,
            max_clients,
            max_bytes,
        }
    }

    fn start(&mut self, client_addr: SocketAddr, generation: u64, datagram: Vec<u8>) -> bool {
        if self.selections.len() >= self.max_clients
            || self.total_bytes.saturating_add(datagram.len()) > self.max_bytes
        {
            return false;
        }
        self.total_bytes += datagram.len();
        self.selections.insert(
            client_addr,
            PendingSelection {
                generation,
                datagrams: VecDeque::from([datagram]),
                overflow_logged: false,
            },
        );
        true
    }

    fn enqueue(&mut self, client_addr: SocketAddr, datagram: Vec<u8>) -> EnqueueResult {
        let Some(selection) = self.selections.get_mut(&client_addr) else {
            return EnqueueResult::GloballyFull;
        };
        if selection.datagrams.len() >= MAX_PENDING_DATAGRAMS {
            let first_drop = !selection.overflow_logged;
            selection.overflow_logged = true;
            return EnqueueResult::PerClientFull { first_drop };
        }
        if self.total_bytes.saturating_add(datagram.len()) > self.max_bytes {
            return EnqueueResult::GloballyFull;
        }
        self.total_bytes += datagram.len();
        selection.datagrams.push_back(datagram);
        EnqueueResult::Queued
    }

    fn contains(&self, client_addr: &SocketAddr) -> bool {
        self.selections.contains_key(client_addr)
    }

    fn is_current(&self, client_addr: &SocketAddr, generation: u64) -> bool {
        self.selections
            .get(client_addr)
            .is_some_and(|selection| selection.generation == generation)
    }

    fn remove(&mut self, client_addr: SocketAddr) -> Option<PendingSelection> {
        let selection = self.selections.remove(&client_addr)?;
        let removed_bytes: usize = selection.datagrams.iter().map(Vec::len).sum();
        self.total_bytes = self.total_bytes.saturating_sub(removed_bytes);
        Some(selection)
    }
}

struct SelectionResult {
    client_addr: SocketAddr,
    generation: u64,
    target_addr: Option<SocketAddr>,
}

/// Runs a UDP relay. Single-target redirects retain generic UDP behavior and
/// forward immediately. Multi-target redirects probe each target in order for
/// each new client and pin the resulting session to the first responder.
pub async fn run(config: UdpProxyConfig) -> io::Result<()> {
    validate_config(&config)?;

    let listener = UdpSocket::bind(config.listen_addr).await?;
    run_with_socket(config, listener).await
}

/// Runs a UDP relay using an already-bound listener. This is useful to reserve
/// an ephemeral address before starting the relay and avoids bind races.
pub async fn run_with_socket(mut config: UdpProxyConfig, listener: UdpSocket) -> io::Result<()> {
    validate_config(&config)?;
    config.listen_addr = listener.local_addr()?;
    run_loop(config, Arc::new(listener)).await
}

fn validate_config(config: &UdpProxyConfig) -> io::Result<()> {
    if config.targets.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "udp redirect requires at least one target",
        ));
    }
    Ok(())
}

async fn run_loop(config: UdpProxyConfig, listener: Arc<UdpSocket>) -> io::Result<()> {
    info!(name = %config.name, listen = %config.listen_addr, targets = ?config.targets, "listening (udp)");

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let mut fake_sessions: HashMap<SocketAddr, FakeSession> = HashMap::new();
    let mut pending = PendingState::new(MAX_PENDING_CLIENTS, MAX_PENDING_BYTES);
    let mut global_pending_limit_logged = false;
    let mut next_generation = 0u64;
    let (selection_tx, mut selection_rx) = mpsc::unbounded_channel::<SelectionResult>();

    let target_reachable = (config.targets.len() == 1)
        .then(|| {
            config
                .bedrock_offline
                .as_ref()
                .map(|_| spawn_reachability_prober(config.targets[0], config.probe_timeout))
        })
        .flatten();

    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        tokio::select! {
            received = listener.recv_from(&mut buf) => {
                let (n, client_addr) = received?;
                let datagram = &buf[..n];

                fake_sessions.retain(|_, session| session.last_activity.elapsed() < FAKE_SESSION_IDLE_TIMEOUT);
                if fake_sessions.contains_key(&client_addr) {
                    handle_offline_datagram(&listener, &config, &mut fake_sessions, client_addr, datagram).await;
                    continue;
                }

                if let Some(reachable) = &target_reachable {
                    if !reachable.load(Ordering::Relaxed)
                        && handle_offline_datagram(&listener, &config, &mut fake_sessions, client_addr, datagram).await
                    {
                        continue;
                    }
                }

                if let Some(socket) = existing_session(&sessions, client_addr).await {
                    if let Err(err) = socket.send(datagram).await {
                        warn!(client = %client_addr, %err, "failed to forward udp datagram to target");
                    }
                    continue;
                }

                if config.targets.len() == 1 {
                    match create_session(&sessions, &listener, &config, client_addr, config.targets[0]).await {
                        Ok(socket) => {
                            if let Err(err) = socket.send(datagram).await {
                                warn!(client = %client_addr, %err, "failed to forward udp datagram to target");
                            }
                        }
                        Err(err) => warn!(client = %client_addr, %err, "failed to open udp session"),
                    }
                    continue;
                }

                if pending.contains(&client_addr) {
                    match pending.enqueue(client_addr, datagram.to_vec()) {
                        EnqueueResult::Queued => {}
                        EnqueueResult::PerClientFull { first_drop: true } => {
                            warn!(client = %client_addr, limit = MAX_PENDING_DATAGRAMS, "dropping udp datagrams while target selection queue is full");
                        }
                        EnqueueResult::PerClientFull { first_drop: false } => {}
                        EnqueueResult::GloballyFull if !global_pending_limit_logged => {
                            global_pending_limit_logged = true;
                            warn!(limit_bytes = MAX_PENDING_BYTES, "dropping udp datagrams while global target selection queue is full");
                        }
                        EnqueueResult::GloballyFull => {}
                    }
                    continue;
                }

                next_generation = next_generation.wrapping_add(1);
                let generation = next_generation;
                if !pending.start(client_addr, generation, datagram.to_vec()) {
                    if !global_pending_limit_logged {
                        global_pending_limit_logged = true;
                        warn!(limit_clients = MAX_PENDING_CLIENTS, limit_bytes = MAX_PENDING_BYTES, "dropping udp client while target selection capacity is full");
                    }
                    continue;
                }
                debug!(client = %client_addr, targets = config.targets.len(), "selecting udp target");
                spawn_target_selection(
                    selection_tx.clone(),
                    client_addr,
                    generation,
                    config.targets.clone(),
                    config.probe_timeout,
                );
            }
            Some(result) = selection_rx.recv() => {
                let is_current = pending.is_current(&result.client_addr, result.generation);
                if !is_current {
                    debug!(client = %result.client_addr, generation = result.generation, "ignoring stale udp target selection");
                    continue;
                }
                let queued = pending.remove(result.client_addr).unwrap().datagrams;
                global_pending_limit_logged = false;

                if let Some(target_addr) = result.target_addr {
                    match create_session(&sessions, &listener, &config, result.client_addr, target_addr).await {
                        Ok(socket) => {
                            debug!(client = %result.client_addr, target = %target_addr, "udp target selected");
                            flush_queued_datagrams(
                                &socket,
                                queued,
                                result.client_addr,
                                target_addr,
                            )
                            .await;
                        }
                        Err(err) => warn!(client = %result.client_addr, target = %target_addr, %err, "failed to open selected udp session"),
                    }
                } else {
                    debug!(client = %result.client_addr, "all udp targets unreachable");
                    if config.bedrock_offline.is_some() {
                        for datagram in queued {
                            handle_offline_datagram(&listener, &config, &mut fake_sessions, result.client_addr, &datagram).await;
                        }
                    }
                }
            }
        }
    }
}

fn spawn_target_selection(
    tx: mpsc::UnboundedSender<SelectionResult>,
    client_addr: SocketAddr,
    generation: u64,
    targets: Vec<SocketAddr>,
    probe_timeout: Duration,
) {
    tokio::spawn(async move {
        let mut selected = None;
        for target_addr in targets {
            if bedrock_offline::probe_target(target_addr, probe_timeout).await {
                selected = Some(target_addr);
                break;
            }
            debug!(client = %client_addr, target = %target_addr, "udp target probe failed");
        }
        let _ = tx.send(SelectionResult {
            client_addr,
            generation,
            target_addr: selected,
        });
    });
}

async fn flush_queued_datagrams(
    socket: &UdpSocket,
    queued: VecDeque<Vec<u8>>,
    client_addr: SocketAddr,
    target_addr: SocketAddr,
) {
    for datagram in queued {
        if let Err(err) = socket.send(&datagram).await {
            warn!(client = %client_addr, target = %target_addr, %err, "failed to forward queued udp datagram to target");
        }
    }
}

async fn handle_offline_datagram(
    listener: &Arc<UdpSocket>,
    config: &UdpProxyConfig,
    fake_sessions: &mut HashMap<SocketAddr, FakeSession>,
    client_addr: SocketAddr,
    datagram: &[u8],
) -> bool {
    let Some(offline) = &config.bedrock_offline else {
        return false;
    };

    if bedrock_offline::is_unconnected_ping(datagram) {
        if let Some(pong) =
            bedrock_offline::build_unconnected_pong(datagram, offline, config.listen_addr.port())
        {
            if let Err(err) = listener.send_to(&pong, client_addr).await {
                warn!(client = %client_addr, %err, "failed to send bedrock offline pong");
            }
        }
        return true;
    }

    let is_handshake_start = matches!(datagram.first(), Some(0x05 | 0x07));
    if !is_handshake_start && !fake_sessions.contains_key(&client_addr) {
        return false;
    }

    if !can_start_fake_session(fake_sessions, client_addr) {
        debug!(client = %client_addr, limit = MAX_FAKE_SESSIONS, "bedrock offline fake-session capacity reached");
        return true;
    }
    let session = fake_sessions
        .entry(client_addr)
        .or_insert_with(bedrock_offline::new_session);
    let replies = bedrock_offline::handle_datagram(
        session,
        datagram,
        client_addr,
        offline.server_guid,
        &offline.motd_line1,
        &offline.motd_line2,
    );
    let done = session.is_done();
    for reply in replies {
        if let Err(err) = listener.send_to(&reply, client_addr).await {
            warn!(client = %client_addr, %err, "failed to send bedrock handshake reply");
        }
    }
    if done {
        fake_sessions.remove(&client_addr);
    }
    true
}

fn can_start_fake_session(
    fake_sessions: &HashMap<SocketAddr, FakeSession>,
    client_addr: SocketAddr,
) -> bool {
    fake_sessions.contains_key(&client_addr) || fake_sessions.len() < MAX_FAKE_SESSIONS
}

fn spawn_reachability_prober(target_addr: SocketAddr, probe_timeout: Duration) -> Arc<AtomicBool> {
    let reachable = Arc::new(AtomicBool::new(false));
    let flag = reachable.clone();
    tokio::spawn(async move {
        loop {
            flag.store(
                bedrock_offline::probe_target(target_addr, probe_timeout).await,
                Ordering::Relaxed,
            );
            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    });
    reachable
}

async fn existing_session(sessions: &Sessions, client_addr: SocketAddr) -> Option<Arc<UdpSocket>> {
    let sessions_guard = sessions.lock().await;
    let session = sessions_guard.get(&client_addr)?;
    *session.last_seen.lock().unwrap() = Instant::now();
    Some(session.socket.clone())
}

async fn create_session(
    sessions: &Sessions,
    listener: &Arc<UdpSocket>,
    config: &UdpProxyConfig,
    client_addr: SocketAddr,
    target_addr: SocketAddr,
) -> io::Result<Arc<UdpSocket>> {
    let bind_addr = if target_addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let target_socket = Arc::new(UdpSocket::bind(bind_addr).await?);
    target_socket.connect(target_addr).await?;
    let last_seen = Arc::new(StdMutex::new(Instant::now()));
    let tracking = connections::track(&config.name, "udp", client_addr, target_addr);

    let mut sessions_guard = sessions.lock().await;
    if let Some(existing) = sessions_guard.get(&client_addr) {
        return Ok(existing.socket.clone());
    }
    sessions_guard.insert(
        client_addr,
        Session {
            socket: target_socket.clone(),
            last_seen: last_seen.clone(),
            _tracking: tracking,
        },
    );
    drop(sessions_guard);

    info!(client = %client_addr, target = %target_addr, "client connected");
    spawn_return_path(
        listener.clone(),
        target_socket.clone(),
        client_addr,
        sessions.clone(),
        last_seen,
        config.idle_timeout,
    );
    Ok(target_socket)
}

fn spawn_return_path(
    listener: Arc<UdpSocket>,
    target_socket: Arc<UdpSocket>,
    client_addr: SocketAddr,
    sessions: Sessions,
    last_seen: Arc<StdMutex<Instant>>,
    idle_timeout: Duration,
) {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let reason = loop {
            match timeout(idle_timeout, target_socket.recv(&mut buf)).await {
                Ok(Ok(n)) => {
                    *last_seen.lock().unwrap() = Instant::now();
                    if let Err(err) = listener.send_to(&buf[..n], client_addr).await {
                        warn!(client = %client_addr, %err, "failed to forward udp datagram to client");
                        break "forward to client failed";
                    }
                }
                Ok(Err(err)) => {
                    debug!(client = %client_addr, %err, "udp target socket closed");
                    break "target unreachable or closed";
                }
                Err(_) => {
                    if remove_current_session(
                        &sessions,
                        client_addr,
                        &target_socket,
                        Some(idle_timeout),
                    )
                    .await
                    {
                        break "idle timeout";
                    }
                }
            }
        };
        info!(client = %client_addr, duration = ?started.elapsed(), reason, "client disconnected");
        remove_current_session(&sessions, client_addr, &target_socket, None).await;
    });
}

async fn remove_current_session(
    sessions: &Sessions,
    client_addr: SocketAddr,
    target_socket: &Arc<UdpSocket>,
    idle_timeout: Option<Duration>,
) -> bool {
    let mut guard = sessions.lock().await;
    let should_remove = guard.get(&client_addr).is_some_and(|session| {
        Arc::ptr_eq(&session.socket, target_socket)
            && idle_timeout
                .is_none_or(|timeout| session.last_seen.lock().unwrap().elapsed() >= timeout)
    });
    if should_remove {
        guard.remove(&client_addr);
    }
    should_remove
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_socket() -> Arc<UdpSocket> {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap())
    }

    fn test_session(
        socket: Arc<UdpSocket>,
        last_seen: Arc<StdMutex<Instant>>,
        client_addr: SocketAddr,
    ) -> Session {
        Session {
            socket,
            last_seen,
            _tracking: connections::track(
                "udp-cleanup-test",
                "udp",
                client_addr,
                "127.0.0.1:9".parse().unwrap(),
            ),
        }
    }

    #[test]
    fn pending_state_rejects_clients_past_global_limit() {
        let mut state = PendingState::new(2, 1024);
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:2".parse().unwrap();
        let c: SocketAddr = "127.0.0.1:3".parse().unwrap();

        assert!(state.start(a, 1, vec![1]));
        assert!(state.start(b, 2, vec![2]));
        assert!(!state.start(c, 3, vec![3]));
    }

    #[test]
    fn pending_state_rejects_datagrams_past_global_byte_limit() {
        let mut state = PendingState::new(10, 3);
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:2".parse().unwrap();

        assert!(state.start(a, 1, vec![1, 2]));
        assert!(!state.start(b, 2, vec![3, 4]));
        let removed = state.remove(a).unwrap();
        assert_eq!(removed.datagrams.len(), 1);
        assert!(state.start(b, 2, vec![3, 4]));
    }

    #[test]
    fn pending_state_caps_each_client_at_64_datagrams() {
        let mut state = PendingState::new(10, 1024);
        let client: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(state.start(client, 1, vec![0]));
        for _ in 1..MAX_PENDING_DATAGRAMS {
            assert!(matches!(
                state.enqueue(client, vec![0]),
                EnqueueResult::Queued
            ));
        }

        assert!(matches!(
            state.enqueue(client, vec![0]),
            EnqueueResult::PerClientFull { first_drop: true }
        ));
        assert!(matches!(
            state.enqueue(client, vec![0]),
            EnqueueResult::PerClientFull { first_drop: false }
        ));
        assert_eq!(state.selections[&client].datagrams.len(), 64);
    }

    #[test]
    fn pending_state_accounts_for_enqueued_and_removed_bytes() {
        let mut state = PendingState::new(10, 5);
        let client: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(state.start(client, 1, vec![1, 2]));
        assert_eq!(state.total_bytes, 2);
        assert!(matches!(
            state.enqueue(client, vec![3, 4, 5]),
            EnqueueResult::Queued
        ));
        assert_eq!(state.total_bytes, 5);
        assert!(matches!(
            state.enqueue(client, vec![6]),
            EnqueueResult::GloballyFull
        ));
        assert_eq!(state.total_bytes, 5);

        state.remove(client).unwrap();
        assert_eq!(state.total_bytes, 0);
    }

    #[test]
    fn fake_session_capacity_allows_existing_client_but_rejects_new_one() {
        let mut fake_sessions = HashMap::new();
        for port in 1..=MAX_FAKE_SESSIONS as u16 {
            fake_sessions.insert(
                SocketAddr::from(([127, 0, 0, 1], port)),
                bedrock_offline::new_session(),
            );
        }
        let existing = SocketAddr::from(([127, 0, 0, 1], 1));
        let newcomer = SocketAddr::from(([127, 0, 0, 1], 20_000));

        assert!(can_start_fake_session(&fake_sessions, existing));
        assert!(!can_start_fake_session(&fake_sessions, newcomer));
    }

    #[tokio::test]
    async fn queued_flush_continues_after_oversized_send_failure() {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target_addr = receiver.local_addr().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.connect(target_addr).await.unwrap();
        let client_addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let queued = VecDeque::from([vec![0; 70_000], b"after-failure".to_vec()]);

        flush_queued_datagrams(&sender, queued, client_addr, target_addr).await;

        let mut buf = [0u8; 32];
        let n = timeout(Duration::from_secs(1), receiver.recv(&mut buf))
            .await
            .expect("later datagram was not attempted")
            .unwrap();
        assert_eq!(&buf[..n], b"after-failure");
    }

    #[tokio::test]
    async fn idle_cleanup_keeps_matching_session_when_activity_resumed() {
        let client_addr: SocketAddr = "127.0.0.1:30001".parse().unwrap();
        let socket = test_socket().await;
        let last_seen = Arc::new(StdMutex::new(Instant::now() - Duration::from_secs(2)));
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::from([(
            client_addr,
            test_session(socket.clone(), last_seen.clone(), client_addr),
        )])));
        *last_seen.lock().unwrap() = Instant::now();

        let removed = remove_current_session(
            &sessions,
            client_addr,
            &socket,
            Some(Duration::from_secs(1)),
        )
        .await;

        assert!(!removed);
        assert!(sessions.lock().await.contains_key(&client_addr));
    }

    #[tokio::test]
    async fn idle_cleanup_removes_truly_idle_matching_session() {
        let client_addr: SocketAddr = "127.0.0.1:30002".parse().unwrap();
        let socket = test_socket().await;
        let last_seen = Arc::new(StdMutex::new(Instant::now() - Duration::from_secs(2)));
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::from([(
            client_addr,
            test_session(socket.clone(), last_seen, client_addr),
        )])));

        let removed = remove_current_session(
            &sessions,
            client_addr,
            &socket,
            Some(Duration::from_secs(1)),
        )
        .await;

        assert!(removed);
        assert!(!sessions.lock().await.contains_key(&client_addr));
    }

    #[tokio::test]
    async fn cleanup_from_stale_socket_cannot_remove_replacement_session() {
        let client_addr: SocketAddr = "127.0.0.1:30003".parse().unwrap();
        let stale_socket = test_socket().await;
        let replacement_socket = test_socket().await;
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::from([(
            client_addr,
            test_session(
                replacement_socket.clone(),
                Arc::new(StdMutex::new(Instant::now())),
                client_addr,
            ),
        )])));

        let removed = remove_current_session(&sessions, client_addr, &stale_socket, None).await;

        assert!(!removed);
        let guard = sessions.lock().await;
        assert!(Arc::ptr_eq(
            &guard.get(&client_addr).unwrap().socket,
            &replacement_socket
        ));
    }
}
