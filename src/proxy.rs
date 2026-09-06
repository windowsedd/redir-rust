use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use crate::connections;
#[cfg(not(unix))]
use crate::plugin::Direction;
use crate::plugin::{probe_first_packet, FailureAction, FailureContext, PacketKind, Plugin};
use crate::shaping::ShapingConfig;

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub name: String,
    pub listen_addr: SocketAddr,
    pub targets: Vec<SocketAddr>,
    pub connect_timeout: Duration,
    pub shaping: Option<ShapingConfig>,
    worker_executable: Option<std::path::PathBuf>,
}

impl ProxyConfig {
    pub fn new(
        name: impl Into<String>,
        listen_addr: SocketAddr,
        targets: Vec<SocketAddr>,
        connect_timeout: Duration,
        shaping: Option<ShapingConfig>,
    ) -> Self {
        Self {
            name: name.into(),
            listen_addr,
            targets,
            connect_timeout,
            shaping,
            worker_executable: None,
        }
    }

    pub fn with_worker_executable(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.worker_executable = Some(path.into());
        self
    }
}

pub async fn run(config: ProxyConfig, plugins: Vec<Arc<dyn Plugin>>) -> io::Result<()> {
    validate_config(&config)?;
    let listener = TcpListener::bind(config.listen_addr).await?;
    run_with_listener(config, listener, plugins).await
}

pub async fn run_with_listener(
    mut config: ProxyConfig,
    listener: TcpListener,
    plugins: Vec<Arc<dyn Plugin>>,
) -> io::Result<()> {
    validate_config(&config)?;
    config.listen_addr = listener.local_addr()?;
    run_loop(config, listener, plugins).await
}

fn validate_config(config: &ProxyConfig) -> io::Result<()> {
    if config.targets.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TCP proxy requires at least one target",
        ));
    }
    Ok(())
}

async fn run_loop(
    config: ProxyConfig,
    listener: TcpListener,
    plugins: Vec<Arc<dyn Plugin>>,
) -> io::Result<()> {
    info!(name = %config.name, listen = %config.listen_addr, targets = ?config.targets, "listening");

    let active_plugins: Vec<Arc<dyn Plugin>> = plugins
        .into_iter()
        .filter(|p| p.applies_to(config.listen_addr))
        .collect();

    for plugin in &active_plugins {
        debug!(plugin = plugin.name(), "plugin active for this listener");
    }

    loop {
        let (client, client_addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                warn!(%err, "failed to accept connection");
                continue;
            }
        };

        let config = config.clone();
        let plugins = active_plugins.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(client, client_addr, &config, &plugins).await {
                debug!(client = %client_addr, %err, "connection ended with error");
            }
        });
    }
}

async fn handle_connection(
    mut client: TcpStream,
    client_addr: SocketAddr,
    config: &ProxyConfig,
    plugins: &[Arc<dyn Plugin>],
) -> io::Result<()> {
    // Probe the first packet to detect ping vs join without consuming bytes.
    let (packet_kind, _is_mc, _consume) = probe_first_packet(&mut client).await;
    match packet_kind {
        Some(PacketKind::StatusPing) => debug!(client = %client_addr, "ping"),
        Some(PacketKind::LoginAttempt) => info!(client = %client_addr, "join"),
        None => debug!(client = %client_addr, "connected"),
    }

    for plugin in plugins {
        if !plugin.on_connect(client_addr).await {
            debug!(client = %client_addr, plugin = plugin.name(), "connection rejected by plugin");
            return Ok(());
        }
    }

    let mut connected = None;
    let mut final_failure = None;
    for &target_addr in &config.targets {
        let connect_result = timeout(config.connect_timeout, TcpStream::connect(target_addr)).await;
        match connect_result {
            Ok(Ok(stream)) => {
                connected = Some((stream, target_addr));
                break;
            }
            Ok(Err(err)) => {
                let error = err.to_string();
                debug!(client = %client_addr, target = %target_addr, %error, "target connection attempt failed");
                final_failure = Some((target_addr, error));
            }
            Err(_) => {
                let error = "connection timed out".to_string();
                debug!(client = %client_addr, target = %target_addr, %error, "target connection attempt failed");
                final_failure = Some((target_addr, error));
            }
        }
    }

    let (target, target_addr) = match connected {
        Some(connected) => connected,
        None => {
            let Some((target_addr, error)) = final_failure else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "TCP proxy requires at least one target",
                ));
            };
            return handle_target_failure(client, client_addr, config, plugins, target_addr, error)
                .await;
        }
    };

    info!(client = %client_addr, target = %target_addr, "connected, proxying");

    let _tracking = connections::track(&config.name, "tcp", client_addr, target_addr);
    let started = Instant::now();
    let result = crate::conn_worker::run(
        client,
        target,
        plugins,
        config.shaping.as_ref(),
        config.worker_executable.as_deref(),
    )
    .await;
    match &result {
        Ok(()) => {
            info!(client = %client_addr, target = %target_addr, duration = ?started.elapsed(), "client disconnected")
        }
        Err(err) => {
            warn!(client = %client_addr, target = %target_addr, duration = ?started.elapsed(), %err, "client disconnected with error")
        }
    }
    result
}

async fn handle_target_failure(
    mut client: TcpStream,
    client_addr: SocketAddr,
    config: &ProxyConfig,
    plugins: &[Arc<dyn Plugin>],
    target_addr: SocketAddr,
    error: String,
) -> io::Result<()> {
    warn!(client = %client_addr, target = %target_addr, %error, "all targets unreachable");
    let ctx = FailureContext {
        client_addr,
        listen_addr: config.listen_addr,
        target_addr,
        error,
    };
    for plugin in plugins {
        match plugin.on_target_failure(&ctx, &mut client).await {
            FailureAction::Handled => {
                debug!(client = %client_addr, plugin = plugin.name(), "target failure handled by plugin");
                return Ok(());
            }
            FailureAction::PassThrough => continue,
        }
    }
    client.shutdown().await.ok();
    Ok(())
}

#[cfg(not(unix))]
const BUF_SIZE: usize = 16 * 1024;

#[cfg(not(unix))]
pub(crate) async fn pipe(
    mut client: TcpStream,
    mut target: TcpStream,
    plugins: &[Arc<dyn Plugin>],
    shaping: Option<&ShapingConfig>,
) -> io::Result<()> {
    let (mut client_rd, mut client_wr) = client.split();
    let (mut target_rd, mut target_wr) = target.split();
    let client_to_target = forward(
        &mut client_rd,
        &mut target_wr,
        Direction::ClientToTarget,
        plugins,
        shaping,
    );
    let target_to_client = forward(
        &mut target_rd,
        &mut client_wr,
        Direction::TargetToClient,
        plugins,
        shaping,
    );
    tokio::select! {
        result = client_to_target => result,
        result = target_to_client => result,
    }
}

#[cfg(not(unix))]
async fn forward<R, W>(
    reader: &mut R,
    writer: &mut W,
    direction: Direction,
    plugins: &[Arc<dyn Plugin>],
    shaping: Option<&ShapingConfig>,
) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use crate::shaping::{Jitter, RateLimiter};
    use tokio::io::AsyncReadExt;

    let applies = shaping.is_some_and(|s| s.wait_in_out.applies_to(direction));
    let bufsize = shaping.map(|s| s.bufsize).unwrap_or(BUF_SIZE);
    let mut limiter = applies
        .then(|| shaping.and_then(|s| s.max_bandwidth_bps))
        .flatten()
        .map(RateLimiter::new);
    let mut jitter = applies
        .then(|| shaping.and_then(|s| s.random_wait_ms))
        .flatten()
        .map(|_| Jitter::new());

    let mut buf = vec![0u8; bufsize.max(1)];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let mut chunk = &buf[..n];
        let mut rewritten: Option<Vec<u8>> = None;
        for plugin in plugins {
            let current = rewritten.as_deref().unwrap_or(chunk);
            if let Some(replaced) = plugin.on_data_receive(direction, current).await {
                rewritten = Some(replaced);
            }
        }
        if let Some(data) = &rewritten {
            chunk = data;
        }
        writer.write_all(chunk).await?;
        if let Some(limiter) = &mut limiter {
            tokio::time::sleep(limiter.wait_for(n)).await;
        }
        if let (Some(jitter), Some(max_ms)) = (&mut jitter, shaping.and_then(|s| s.random_wait_ms))
        {
            tokio::time::sleep(jitter.random_wait(max_ms)).await;
        }
    }
    writer.shutdown().await.ok();
    Ok(())
}
