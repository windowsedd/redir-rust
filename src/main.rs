#[allow(unused_variables, dead_code)]
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use redir_rust::config::{self, FileConfig, Protocol, RedirectConfig};
use redir_rust::plugin::Plugin;
use redir_rust::plugins::MinecraftOfflinePlugin;
use redir_rust::proxy::{self, ProxyConfig};
use redir_rust::udp_proxy::{self, UdpProxyConfig};
use redir_rust::{conn_worker, edit_config, install, notify, service_ctl, status};

/// A Rust port redirector with plugin support, inspired by `redir`.
///
/// Either pass `--config <FILE>` to run one or more redirects defined in a
/// TOML file, or pass `--listen` and one or more `--target` flags (plus other
/// flags) to run a single redirect directly from the command line.
#[derive(Parser, Debug)]
#[command(
    name = "redir-rust",
    version = redir_rust::version::SHORT,
    long_version = redir_rust::version::LONG,
    about
)]
struct Cli {
    /// Path to a TOML config file defining one or more [[redirect]] entries.
    /// When set, all other redirect flags below are ignored.
    #[arg(short = 'c', long = "config")]
    config: Option<std::path::PathBuf>,

    /// Install this binary as a systemd service (copies itself to
    /// /usr/local/bin, writes /etc/redir-rust/config.toml if missing, and
    /// installs + enables the unit file). Requires root. Linux only.
    #[arg(long = "install-systemd")]
    install_systemd: bool,

    /// Deploy a freshly built binary over an existing install: copies
    /// itself to /usr/local/bin and restarts the unit (--unit), without
    /// touching the config or unit file. Requires root. Linux only.
    #[arg(long = "update")]
    update: bool,

    /// Print `systemctl status` for the unit (--unit), as-is. Prints and
    /// exits; does not start any redirects.
    #[arg(long = "service-status", visible_alias = "status")]
    service_status: bool,

    /// Start the systemd unit (`systemctl start <unit>`). Requires root.
    #[arg(long = "start")]
    service_start: bool,

    /// Stop the systemd unit (`systemctl stop <unit>`). Requires root.
    #[arg(long = "stop")]
    service_stop: bool,

    /// Restart the systemd unit (`systemctl restart <unit>`). Requires root.
    #[arg(long = "restart")]
    service_restart: bool,

    /// Unit name used by --service-status/--start/--stop/--restart.
    #[arg(long = "unit", default_value = "redir-rust.service")]
    unit: String,

    /// Open the config file (--config, default /etc/redir-rust/config.toml)
    /// in $EDITOR, creating it from the default template first if it
    /// doesn't exist, then re-validate it after the editor exits. Does not
    /// restart the service.
    #[arg(short = 'e', long = "edit-config")]
    edit_config: bool,

    /// Optional label used in logs to identify this redirect.
    #[arg(short = 'n', long = "name")]
    name: Option<String>,

    /// Local address to listen on, e.g. 0.0.0.0:25565
    #[arg(short = 'l', long = "listen", required_unless_present_any = ["config", "install_systemd", "update", "service_status", "service_start", "service_stop", "service_restart", "edit_config"])]
    listen: Option<SocketAddr>,

    /// Backend target address to forward connections to, in priority order.
    /// Repeat for additional targets, e.g. `-t 127.0.0.1:25566 -t 127.0.0.1:25567`.
    #[arg(short = 't', long = "target", value_name = "TARGET", required_unless_present_any = ["config", "install_systemd", "update", "service_status", "service_start", "service_stop", "service_restart", "edit_config"])]
    targets: Vec<SocketAddr>,

    /// Transport to relay: "tcp" (default) or "udp".
    #[arg(short = 'p', long = "protocol", default_value = "tcp")]
    protocol: Protocol,

    /// TCP only: timeout in milliseconds when connecting to the backend target.
    #[arg(long = "connect-timeout", default_value_t = 3000)]
    connect_timeout_ms: u64,

    /// UDP only: idle timeout in milliseconds before a client's session is torn down.
    #[arg(long = "udp-idle-timeout", default_value_t = 60_000)]
    udp_idle_timeout_ms: u64,

    /// Enable the built-in Minecraft "server offline" MOTD plugin. When the
    /// backend target is unreachable, connecting Minecraft clients receive a
    /// custom offline status instead of a connection refusal.
    #[arg(long = "minecraft-offline-motd")]
    minecraft_offline_motd: bool,

    /// Overrides the top-right status text (SLP `version.name`). Defaults to
    /// a bilingual "server unreachable" message.
    #[arg(long = "status-line")]
    status_line: Option<String>,

    /// Overrides MOTD line 1. Defaults to a bilingual "server not found" message.
    #[arg(long = "motd-line1")]
    motd_line1: Option<String>,

    /// Overrides MOTD line 2. Defaults to a bilingual "check your connection domain" message.
    #[arg(long = "motd-line2")]
    motd_line2: Option<String>,

    /// Path to a custom 64x64 PNG favicon. Defaults to the built-in scroll icon.
    #[arg(long = "favicon-path")]
    favicon_path: Option<std::path::PathBuf>,

    /// UDP only: enable the built-in Bedrock "server offline" MOTD. When the
    /// backend target isn't responding, RakNet pings get a canned offline
    /// server-list entry instead of just timing out.
    #[arg(long = "bedrock-offline-motd")]
    bedrock_offline_motd: bool,

    /// Overrides Bedrock MOTD line 1 (server name). Defaults to a bilingual
    /// "server unreachable" message.
    #[arg(long = "bedrock-motd-line1")]
    bedrock_motd_line1: Option<String>,

    /// Overrides Bedrock MOTD line 2 (sub-line). Defaults to a bilingual
    /// "check your connection domain" message.
    #[arg(long = "bedrock-motd-line2")]
    bedrock_motd_line2: Option<String>,

    /// Shorthand for `RUST_LOG=debug`, without needing to set the env var.
    /// Ignored if RUST_LOG is already set (that takes precedence).
    #[arg(long = "debug")]
    debug: bool,

    /// TCP only: bandwidth cap in bits/second. Unset means unlimited.
    #[arg(long = "max-bandwidth")]
    max_bandwidth_bps: Option<u64>,

    /// TCP only: which direction(s) --max-bandwidth/--random-wait apply to.
    #[arg(long = "wait-in-out", default_value = "both")]
    wait_in_out: redir_rust::shaping::WaitInOut,

    /// TCP only: upper bound (milliseconds) of a random delay applied per
    /// chunk, in the direction(s) selected by --wait-in-out.
    #[arg(long = "random-wait")]
    random_wait_ms: Option<u64>,

    /// TCP only: read/write chunk size shaping is applied at.
    #[arg(long = "bufsize", default_value_t = 16 * 1024)]
    bufsize_bytes: usize,
}

fn main() -> ExitCode {
    // Checked before clap parsing and before starting a tokio runtime: this
    // hidden mode is the per-connection worker child spawned by
    // `conn_worker.rs`, not part of the public CLI.
    if conn_worker::is_worker_invocation() {
        return conn_worker::run_worker();
    }
    real_main()
}

#[tokio::main]
async fn real_main() -> ExitCode {
    let cli = Cli::parse();

    let default_level = if cli.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_level)),
        )
        .init();

    if cli.install_systemd {
        return install::run();
    }

    if cli.update {
        return install::update(&cli.unit);
    }

    let config_path_or_default = || {
        cli.config
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("/etc/redir-rust/config.toml"))
    };

    if cli.service_status {
        return status::run(&cli.unit);
    }

    if cli.service_start {
        return service_ctl::run("start", &cli.unit);
    }

    if cli.service_stop {
        return service_ctl::run("stop", &cli.unit);
    }

    if cli.service_restart {
        return service_ctl::run("restart", &cli.unit);
    }

    if cli.edit_config {
        return edit_config::run(&config_path_or_default());
    }

    let redirects = match &cli.config {
        Some(path) => match FileConfig::load(path) {
            Ok(file) => file.redirects,
            Err(err) => {
                tracing::error!(%err, "failed to load config file");
                return ExitCode::FAILURE;
            }
        },
        None => vec![RedirectConfig {
            name: cli.name.clone(),
            listen: cli.listen.expect("clap enforces listen when no config"),
            target_config: config::TargetConfig::from_ordered(cli.targets.clone()),
            protocol: cli.protocol,
            connect_timeout_ms: cli.connect_timeout_ms,
            udp_idle_timeout_ms: cli.udp_idle_timeout_ms,
            minecraft: cli.minecraft_offline_motd.then(|| config::MinecraftConfig {
                plugins: Some(config::MinecraftPluginsConfig {
                    enabled: true,
                    status_line: cli.status_line.clone(),
                    motd_line1: cli.motd_line1.clone(),
                    motd_line2: cli.motd_line2.clone(),
                    favicon_path: cli.favicon_path.clone(),
                }),
            }),
            bedrock: cli.bedrock_offline_motd.then(|| config::BedrockConfig {
                plugins: Some(config::BedrockPluginsConfig {
                    enabled: true,
                    motd_line1: cli.bedrock_motd_line1.clone(),
                    motd_line2: cli.bedrock_motd_line2.clone(),
                }),
            }),
            max_bandwidth_bps: cli.max_bandwidth_bps,
            wait_in_out: cli.wait_in_out,
            random_wait_ms: cli.random_wait_ms,
            bufsize_bytes: cli.bufsize_bytes,
        }],
    };

    let mut tasks = tokio::task::JoinSet::new();
    for redirect in redirects {
        tasks.spawn(run_redirect(redirect));
    }

    // Tells systemd (Type=notify units) we're up, so `systemctl status`
    // moves past "activating" and dependent units can start. No-op if not
    // running under systemd.
    notify::ready();

    // Without this, Ctrl+C falls through to the OS default: the process is
    // killed outright (on Windows, exit code 0xc000013a /
    // STATUS_CONTROL_C_EXIT), which terminals/wrappers like `cargo run`
    // report as a failure even though stopping a foreground test run with
    // Ctrl+C is completely normal. Racing it here lets us log the shutdown
    // and return a clean exit code instead. Dropping `tasks` (JoinSet) on
    // this branch aborts every still-running redirect, which is fine --
    // there's nothing to gracefully drain, just listening sockets to close.
    let failed = tokio::select! {
        failed = wait_for_tasks(&mut tasks) => failed,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl+C, shutting down");
            false
        }
    };

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

async fn wait_for_tasks(tasks: &mut tokio::task::JoinSet<std::io::Result<()>>) -> bool {
    let mut failed = false;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::error!(%err, "redirect exited with error");
                failed = true;
            }
            Err(join_err) => {
                tracing::error!(%join_err, "redirect task panicked");
                failed = true;
            }
        }
    }
    failed
}

async fn run_redirect(redirect: RedirectConfig) -> std::io::Result<()> {
    match redirect.protocol {
        Protocol::Tcp => run_tcp_redirect(redirect).await,
        Protocol::Udp => run_udp_redirect(redirect).await,
    }
}

async fn run_udp_redirect(redirect: RedirectConfig) -> std::io::Result<()> {
    if redirect.minecraft_plugins().is_some() {
        tracing::warn!(
            name = %redirect.display_name(),
            "minecraft plugins apply to TCP redirects only; ignoring for this UDP redirect"
        );
    }

    let bedrock_offline = match redirect.bedrock_plugins() {
        Some(bp) if bp.enabled => Some(
            redir_rust::plugins::bedrock_offline::BedrockOfflineConfig::new(
                bp.motd_line1
                    .clone()
                    .unwrap_or_else(redir_rust::plugins::bedrock_offline::default_motd_line1),
                bp.motd_line2
                    .clone()
                    .unwrap_or_else(redir_rust::plugins::bedrock_offline::default_motd_line2),
            ),
        ),
        _ => None,
    };

    let config = UdpProxyConfig {
        name: redirect.display_name(),
        listen_addr: redirect.listen,
        targets: redirect.target_addrs().to_vec(),
        idle_timeout: redirect.udp_idle_timeout(),
        probe_timeout: Duration::from_secs(2),
        bedrock_offline,
    };

    udp_proxy::run(config).await
}

async fn run_tcp_redirect(redirect: RedirectConfig) -> std::io::Result<()> {
    if redirect.bedrock_plugins().is_some() {
        tracing::warn!(
            name = %redirect.display_name(),
            "bedrock offline motd applies to UDP redirects only; ignoring for this TCP redirect"
        );
    }

    let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();
    if let Some(mc) = redirect.minecraft_plugins() {
        if mc.enabled {
            let mut plugin = MinecraftOfflinePlugin::new(redirect.listen.port());

            if let Some(status_line) = &mc.status_line {
                plugin = plugin.with_status_line(status_line.clone());
            }
            if mc.motd_line1.is_some() || mc.motd_line2.is_some() {
                plugin = plugin.with_motd(
                    mc.motd_line1.clone().unwrap_or_default(),
                    mc.motd_line2.clone().unwrap_or_default(),
                );
            }
            if let Some(favicon_path) = &mc.favicon_path {
                let bytes = std::fs::read(favicon_path).map_err(|err| {
                    std::io::Error::new(
                        err.kind(),
                        format!("failed to read favicon {}: {err}", favicon_path.display()),
                    )
                })?;
                plugin = plugin.with_favicon_png_bytes(Some(&bytes));
            }

            plugins.push(Arc::new(plugin));
        }
    }

    let config = ProxyConfig::new(
        redirect.display_name(),
        redirect.listen,
        redirect.target_addrs().to_vec(),
        redirect.connect_timeout(),
        redirect.shaping(),
    );

    proxy::run(config, plugins).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn accepts_repeated_targets_in_order() {
        let cli = Cli::try_parse_from([
            "redir-rust",
            "--listen",
            "127.0.0.1:25565",
            "--target",
            "127.0.0.1:25566",
            "--target",
            "127.0.0.1:25567",
        ])
        .expect("repeated --target flags should be accepted");

        assert_eq!(
            cli.targets,
            [
                "127.0.0.1:25566".parse::<SocketAddr>().unwrap(),
                "127.0.0.1:25567".parse::<SocketAddr>().unwrap(),
            ]
        );
    }

    #[test]
    fn accepts_one_target() {
        let cli = Cli::try_parse_from([
            "redir-rust",
            "--listen",
            "127.0.0.1:25565",
            "--target",
            "127.0.0.1:25566",
        ])
        .expect("one --target flag should be accepted");

        assert_eq!(
            cli.targets,
            ["127.0.0.1:25566".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn target_help_uses_singular_value_name_and_explains_repetition() {
        let help = Cli::command().render_long_help().to_string();

        assert!(help.contains("--target <TARGET>"));
        assert!(help.contains("Repeat for additional targets"));
    }

    #[test]
    fn rejects_direct_mode_without_target() {
        let error = Cli::try_parse_from(["redir-rust", "--listen", "127.0.0.1:25565"]).unwrap_err();

        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert!(error.to_string().contains("--target"));
    }
}
