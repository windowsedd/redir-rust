//! Prepares config updates before changing live listeners.
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::config::{FileConfig, Protocol, RedirectConfig};
use crate::plugin::Plugin;
use crate::plugins::MinecraftOfflinePlugin;
use crate::proxy::{self, ProxyConfig, TcpSettings};
use crate::udp_proxy::{self, UdpProxyConfig};
use crate::{connections, notify, reload};

// Transport is part of the key: TCP and UDP may share a numeric port.
type Key = (bool, SocketAddr);

enum Settings {
    Tcp(TcpSettings),
    Udp(UdpProxyConfig),
}

enum Updates {
    Tcp(watch::Sender<Option<TcpSettings>>),
    Udp(watch::Sender<Option<UdpProxyConfig>>),
}

impl Updates {
    fn set(&self, next: Option<Settings>) {
        match (self, next) {
            (Self::Tcp(tx), Some(Settings::Tcp(settings))) => {
                tx.send_replace(Some(settings));
            }
            (Self::Udp(tx), Some(Settings::Udp(settings))) => {
                tx.send_replace(Some(settings));
            }
            (Self::Tcp(tx), None) => {
                tx.send_replace(None);
            }
            (Self::Udp(tx), None) => {
                tx.send_replace(None);
            }
            _ => unreachable!("transport is part of the listener key"),
        }
    }
}

enum Bound {
    Tcp(TcpListener),
    Udp(UdpSocket),
}

struct Supervisor {
    active: HashMap<Key, Updates>,
    names: HashSet<String>,
    tasks: JoinSet<io::Result<()>>,
}

impl Supervisor {
    async fn apply(&mut self, redirects: Vec<RedirectConfig>) -> io::Result<()> {
        let mut keys = HashSet::new();
        let mut prepared = Vec::new();
        for redirect in redirects {
            let key = (redirect.protocol == Protocol::Udp, redirect.listen);
            if !keys.insert(key) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "duplicate {} listener {}",
                        if key.0 { "UDP" } else { "TCP" },
                        key.1
                    ),
                ));
            }
            let settings = settings(redirect)?;
            let bound = if self.active.contains_key(&key) {
                None
            } else if key.0 {
                Some(Bound::Udp(
                    UdpSocket::bind(key.1)
                        .await
                        .map_err(|e| bind_error(key, e))?,
                ))
            } else {
                Some(Bound::Tcp(
                    TcpListener::bind(key.1)
                        .await
                        .map_err(|e| bind_error(key, e))?,
                ))
            };
            prepared.push((key, settings, bound));
        }
        // No fallible preparations beyond this point. On any error above,
        // temporary sockets drop and all old listeners/settings stay intact.
        let names: HashSet<_> = prepared
            .iter()
            .map(|(_, settings, _)| match settings {
                Settings::Tcp(s) => s.config.name.clone(),
                Settings::Udp(s) => s.name.clone(),
            })
            .collect();
        for name in self.names.difference(&names) {
            connections::retire_redirect(name);
        }
        self.names = names;
        self.active.retain(|key, updates| {
            if keys.contains(key) {
                true
            } else {
                updates.set(None);
                false
            }
        });
        for (key, settings, bound) in prepared {
            let name = match &settings {
                Settings::Tcp(s) => &s.config.name,
                Settings::Udp(s) => &s.name,
            };
            connections::register_redirect(name, if key.0 { "udp" } else { "tcp" }, key.1);
            match (settings, bound) {
                (settings, None) => self.active[&key].set(Some(settings)),
                (Settings::Tcp(settings), Some(Bound::Tcp(listener))) => {
                    let (tx, rx) = watch::channel(Some(settings));
                    self.active.insert(key, Updates::Tcp(tx));
                    self.tasks.spawn(proxy::run_reconfigurable(listener, rx));
                }
                (Settings::Udp(settings), Some(Bound::Udp(socket))) => {
                    let (tx, rx) = watch::channel(Some(settings));
                    self.active.insert(key, Updates::Udp(tx));
                    self.tasks.spawn(udp_proxy::run_reconfigurable(socket, rx));
                }
                _ => unreachable!("prepared socket matches its transport"),
            }
        }
        Ok(())
    }

    async fn reload(&mut self, config: &Path) -> io::Result<()> {
        let text = std::fs::read_to_string(config)?;
        let parsed: FileConfig = toml::from_str(&text).map_err(io::Error::other)?;
        // Empty is valid for reload: retire listeners but keep control alive.
        let redirects = if parsed.redirects.is_empty() {
            Vec::new()
        } else {
            FileConfig::parse_str(&text)
                .map_err(io::Error::other)?
                .redirects
        };
        self.apply(redirects).await?;
        tracing::info!(
            listeners = self.active.len(),
            "configuration reloaded; established sessions retained"
        );
        Ok(())
    }
}

fn bind_error(key: Key, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!(
            "cannot bind {} {}: {error}; current configuration retained",
            if key.0 { "UDP" } else { "TCP" },
            key.1
        ),
    )
}

/// Runs direct CLI redirects or a reloadable config-based instance.
pub async fn run(redirects: Vec<RedirectConfig>, config: Option<&Path>) -> io::Result<()> {
    let mut supervisor = Supervisor {
        active: HashMap::new(),
        names: HashSet::new(),
        tasks: JoinSet::new(),
    };
    supervisor.apply(redirects).await?;
    let config = config.map(std::fs::canonicalize).transpose()?;
    let control = match config.as_deref() {
        Some(config) => match reload::Server::bind(config).await {
            Ok(server) => Some(server),
            Err(err) => {
                tracing::warn!(%err, "reload control unavailable");
                None
            }
        },
        None => None,
    };
    #[cfg(unix)]
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut authenticating = JoinSet::new();
    notify::ready();
    connections::spawn_snapshot_task();
    loop {
        tokio::select! {
            done = supervisor.tasks.join_next(), if !supervisor.tasks.is_empty() => {
                match done {
                    Some(Ok(Ok(()))) => {}, // A retired listener finished draining.
                    Some(Ok(Err(err))) => return Err(err),
                    Some(Err(err)) => return Err(io::Error::other(err)),
                    None => {},
                }
            }
            accepted = async { control.as_ref().unwrap().listener.accept().await }, if control.is_some() && authenticating.len() < 32 => {
                let (stream, _) = accepted?;
                authenticating.spawn(control.as_ref().unwrap().authenticate(stream));
            }
            done = authenticating.join_next(), if !authenticating.is_empty() => {
                let Some(Ok((mut stream, command))) = done else { continue; };
                let result = match command {
                    Ok(command) if command == "reload" => supervisor.reload(config.as_deref().unwrap()).await,
                    Ok(_) => Ok(()),
                    Err(err) => Err(err),
                };
                if let Err(err) = &result { tracing::warn!(%err, "reload request rejected"); }
                reload::respond(&mut stream, result).await;
            }
            _ = hangup_received(
                #[cfg(unix)] &mut hangup
            ) => {
                if let Some(path) = &config {
                    if let Err(err) = supervisor.reload(path).await {
                        tracing::warn!(%err, "configuration reload rejected; current listeners retained");
                    }
                } else { tracing::warn!("direct CLI redirects have no config file to reload"); }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received Ctrl+C, shutting down");
                return Ok(());
            }
        }
    }
}

#[cfg(unix)]
async fn hangup_received(signal: &mut tokio::signal::unix::Signal) {
    signal.recv().await;
}
#[cfg(not(unix))]
async fn hangup_received() {
    std::future::pending::<()>().await;
}

fn settings(redirect: RedirectConfig) -> io::Result<Settings> {
    let name = redirect.display_name();
    if redirect.protocol == Protocol::Udp {
        if redirect.minecraft_plugins().is_some() {
            tracing::warn!(%name, "minecraft plugins apply to TCP only; ignoring for UDP");
        }
        let bedrock_offline = redirect.bedrock_plugins().filter(|p| p.enabled).map(|p| {
            crate::plugins::bedrock_offline::BedrockOfflineConfig::new(
                p.motd_line1
                    .clone()
                    .unwrap_or_else(crate::plugins::bedrock_offline::default_motd_line1),
                p.motd_line2
                    .clone()
                    .unwrap_or_else(crate::plugins::bedrock_offline::default_motd_line2),
            )
        });
        return Ok(Settings::Udp(UdpProxyConfig {
            name,
            listen_addr: redirect.listen,
            targets: redirect.target_addrs().to_vec(),
            idle_timeout: redirect.udp_idle_timeout(),
            probe_timeout: Duration::from_secs(2),
            bedrock_offline,
        }));
    }
    if redirect.bedrock_plugins().is_some() {
        tracing::warn!(%name, "bedrock offline motd applies to UDP only; ignoring for TCP");
    }
    let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();
    if let Some(mc) = redirect.minecraft_plugins().filter(|p| p.enabled) {
        let mut plugin = MinecraftOfflinePlugin::new(redirect.listen.port());
        if let Some(text) = &mc.status_line {
            plugin = plugin.with_status_line(text);
        }
        if mc.motd_line1.is_some() || mc.motd_line2.is_some() {
            plugin = plugin.with_motd(
                mc.motd_line1
                    .clone()
                    .unwrap_or_else(crate::plugins::minecraft_offline::default_motd_line1),
                mc.motd_line2
                    .clone()
                    .unwrap_or_else(crate::plugins::minecraft_offline::default_motd_line2),
            );
        }
        if let Some(path) = &mc.favicon_path {
            let bytes = std::fs::read(path).map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("failed to read favicon {}: {err}", path.display()),
                )
            })?;
            plugin = plugin.with_favicon_png_bytes(Some(&bytes));
        }
        plugins.push(Arc::new(plugin));
    }
    Ok(Settings::Tcp(TcpSettings {
        config: ProxyConfig::new(
            name,
            redirect.listen,
            redirect.target_addrs().to_vec(),
            redirect.connect_timeout(),
            redirect.shaping(),
        ),
        plugins,
    }))
}
