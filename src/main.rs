#[allow(unused_variables, dead_code)]
use std::net::SocketAddr;
use std::process::ExitCode;

use clap::{parser::ValueSource, CommandFactory, FromArgMatches, Parser};

use redir_rust::config::{self, FileConfig, Protocol, RedirectConfig};
use redir_rust::{config_manager, conn_worker, edit_config, install, service_ctl, status};

mod gui;
mod menu;
mod prompts;
mod setup;
mod tailscale;

/// A Rust port redirector with plugin support, inspired by `redir`.
///
/// Pass `--settings <FILE>` to select a JSON settings file pointing to a TOML
/// config, `--config <FILE>` to select the TOML file directly, or `--listen`
/// and one or more `--target` flags to run a single redirect.
#[derive(Parser, Debug)]
#[command(
    name = "redir-rust",
    version = redir_rust::version::SHORT,
    long_version = redir_rust::version::LONG,
    about
)]
struct Cli {
    /// Open the local graphical manager in your default browser.
    #[arg(long, conflicts_with = "config")]
    gui: bool,
    /// Address for --gui (default: all IPv4 interfaces, automatic port).
    #[arg(long = "gui-bind", value_name = "IP:PORT", requires = "gui")]
    gui_bind: Option<SocketAddr>,
    /// Path to a TOML config file defining one or more [[redirect]] entries.
    /// In run mode, redirect flags cannot be combined with this; with --add, they define the new redirect.
    #[arg(short = 'c', long = "config")]
    config: Option<std::path::PathBuf>,

    /// JSON settings file with a config_path field (default: OS settings directory).
    #[arg(long = "settings")]
    settings: Option<std::path::PathBuf>,

    /// Install this binary as a systemd service (copies itself to
    /// /usr/local/bin, writes /etc/local/redir-rust/config.toml if missing, and
    /// installs + enables the unit file). Requires root. Linux only.
    #[arg(long = "install-systemd")]
    install_systemd: bool,

    /// Deploy a freshly built binary over an existing install: copies
    /// itself to /usr/local/bin and restarts the unit (--unit), without
    /// touching the config or unit file. Requires root. Linux only.
    #[arg(long = "update")]
    update: bool,

    /// Show systemd status on Linux or background-process status on Windows.
    #[arg(long = "service-status", visible_alias = "status")]
    service_status: bool,

    /// Open a btop-style live monitor of the running service's connections and traffic.
    #[arg(long = "monitor")]
    monitor: bool,

    /// Start the systemd unit on Linux or background process on Windows.
    #[arg(long = "start")]
    service_start: bool,

    /// Stop the systemd unit on Linux or background process on Windows.
    #[arg(long = "stop")]
    service_stop: bool,

    /// Restart the systemd unit on Linux or background process on Windows.
    #[arg(long = "restart")]
    service_restart: bool,

    /// Reload the running instance's config while preserving established sessions.
    /// Uses --config or the path from settings.json.
    #[arg(long = "reload", conflicts_with_all = ["add", "remove", "edit", "edit_config", "service_start", "service_stop", "service_restart", "gui", "install_systemd", "update"])]
    reload: bool,

    /// Unit name used by --service-status/--start/--stop/--restart.
    #[arg(long = "unit", default_value = "redir-rust.service")]
    unit: String,

    /// Directory for --install-systemd and --update (default /usr/local/bin).
    #[arg(long = "bin-dir", default_value = "/usr/local/bin")]
    bin_dir: std::path::PathBuf,

    /// Open the config file (--config, otherwise the path in settings.json)
    /// in $EDITOR, creating it from the default template first if it
    /// doesn't exist, then re-validate it after the editor exits. Does not
    /// restart the service.
    #[arg(short = 'e', long = "edit-config")]
    edit_config: bool,

    /// Add a named redirect to the config using --name, --listen, and --target.
    #[arg(long, conflicts_with_all = ["remove", "edit", "edit_config"])]
    add: bool,

    /// Remove a redirect by name from the config.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["add", "edit", "edit_config"])]
    remove: Option<String>,

    /// Open one named redirect in $EDITOR and validate it before saving.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["add", "remove", "edit_config"])]
    edit: Option<String>,

    /// Optional label used in logs to identify this redirect.
    #[arg(short = 'n', long = "name")]
    name: Option<String>,

    /// Local address to listen on, e.g. 0.0.0.0:25565
    #[arg(short = 'l', long = "listen", required_unless_present_any = ["gui", "config", "settings", "install_systemd", "update", "service_status", "monitor", "service_start", "service_stop", "service_restart", "reload", "edit_config", "remove", "edit"])]
    listen: Option<SocketAddr>,

    /// Backend target address to forward connections to, in priority order.
    /// Repeat for additional targets, e.g. `-t 127.0.0.1:25566 -t 127.0.0.1:25567`.
    #[arg(short = 't', long = "target", value_name = "TARGET", required_unless_present_any = ["gui", "config", "settings", "install_systemd", "update", "service_status", "monitor", "service_start", "service_stop", "service_restart", "reload", "edit_config", "remove", "edit"])]
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
    if std::env::args_os().len() == 1 {
        return menu::run();
    }
    real_main()
}

#[tokio::main]
async fn real_main() -> ExitCode {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());

    if (cli.config.is_some() || cli.settings.is_some()) && !cli.add {
        let redirect_flags = [
            "name",
            "listen",
            "targets",
            "protocol",
            "connect_timeout_ms",
            "udp_idle_timeout_ms",
            "minecraft_offline_motd",
            "status_line",
            "motd_line1",
            "motd_line2",
            "favicon_path",
            "bedrock_offline_motd",
            "bedrock_motd_line1",
            "bedrock_motd_line2",
            "max_bandwidth_bps",
            "wait_in_out",
            "random_wait_ms",
            "bufsize_bytes",
        ];
        if redirect_flags
            .iter()
            .any(|id| matches.value_source(id) == Some(ValueSource::CommandLine))
        {
            eprintln!("error: redirect flags cannot be combined with --config in run mode; use --add to save a redirect");
            return ExitCode::FAILURE;
        }
    }

    let default_level = if cli.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_level)),
        )
        .init();

    if cli.install_systemd {
        return install::run(&cli.bin_dir);
    }

    if cli.update {
        return install::update(&cli.unit, &cli.bin_dir);
    }

    if cli.service_status {
        return status::run(&cli.unit);
    }

    if cli.monitor {
        return redir_rust::monitor::run();
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

    let config_path = match &cli.config {
        Some(path) => path.clone(),
        None if cli.settings.is_some()
            || cli.gui
            || cli.reload
            || cli.edit_config
            || cli.add
            || cli.remove.is_some()
            || cli.edit.is_some() =>
        {
            let settings = cli
                .settings
                .clone()
                .unwrap_or_else(config_manager::settings_path);
            match config_manager::configured_path(&settings) {
                Ok(path) => path,
                Err(err) => {
                    eprintln!(
                        "error: failed to read settings {}: {err}",
                        settings.display()
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
        None => config_manager::default_path(),
    };

    if cli.reload {
        return match redir_rust::reload::request(&config_path) {
            Ok(()) => {
                println!("Configuration reloaded; established connections and sessions retained.");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("error: reload failed: {err}");
                ExitCode::FAILURE
            }
        };
    }

    if cli.gui {
        return gui::run(
            &config_path,
            cli.gui_bind.unwrap_or_else(|| "0.0.0.0:0".parse().unwrap()),
        );
    }

    if cli.edit_config {
        return edit_config::run(&config_path);
    }

    if cli.add {
        if cli.name.as_deref().is_none_or(str::is_empty)
            || cli.listen.is_none()
            || cli.targets.is_empty()
        {
            eprintln!("error: --add requires --name, --listen, and at least one --target");
            return ExitCode::FAILURE;
        }
        let name = cli.name.as_deref().unwrap();
        return match config_manager::add(&config_path, cli_redirect(&cli)) {
            Ok(()) => {
                println!("added redirect {name:?}; reload redir-rust to apply it");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        };
    }
    if let Some(name) = &cli.remove {
        return match config_manager::remove(&config_path, name) {
            Ok(last_redirect) => {
                if last_redirect {
                    println!("removed redirect {name:?}; no redirects remain; reload redir-rust to drain its listeners");
                } else {
                    println!("removed redirect {name:?}; reload redir-rust to apply it");
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        };
    }
    if let Some(name) = &cli.edit {
        return match config_manager::edit(&config_path, name) {
            Ok(()) => {
                println!("edited redirect {name:?}; reload redir-rust to apply it");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        };
    }

    let redirects = match cli.config.as_ref().or(cli.settings.as_ref()) {
        Some(_) => match FileConfig::load(&config_path) {
            Ok(file) => file.redirects,
            Err(err) => {
                tracing::error!(%err, "failed to load config file");
                return ExitCode::FAILURE;
            }
        },
        None => vec![cli_redirect(&cli)],
    };

    let reload_path =
        (cli.config.is_some() || cli.settings.is_some()).then_some(config_path.as_path());
    match redir_rust::runtime::run(redirects, reload_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(%err, "redirect exited with error");
            ExitCode::FAILURE
        }
    }
}

fn cli_redirect(cli: &Cli) -> RedirectConfig {
    RedirectConfig {
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
    }
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
