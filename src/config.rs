use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::shaping::{ShapingConfig, WaitInOut};

/// Default config dropped in place by `--install-systemd` and `--edit-config`
/// when no config file exists yet.
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../config.example.toml");

/// Top-level shape of a `config.toml` file: one or more independent redirects,
/// each with its own listen/target pair and plugin settings.
#[derive(Debug, Deserialize)]
pub struct FileConfig {
    #[serde(rename = "redirect", default)]
    pub redirects: Vec<RedirectConfig>,
}

#[derive(Debug, Deserialize)]
pub struct RedirectConfig {
    /// Optional label used in logs to identify this redirect. Defaults to
    /// a description of the listen address and ordered target(s) if unset.
    #[serde(default)]
    pub name: Option<String>,

    pub listen: SocketAddr,

    #[serde(flatten)]
    pub target_config: TargetConfig,

    /// Which transport to relay. Defaults to TCP; set to `"udp"` for a
    /// connectionless relay (e.g. game servers, DNS, voice traffic).
    #[serde(default)]
    pub protocol: Protocol,

    /// TCP only: timeout when connecting to the backend target.
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,

    /// UDP only: how long a client's session is kept alive with no traffic
    /// in either direction before its mapping is torn down.
    #[serde(default = "default_udp_idle_timeout_ms")]
    pub udp_idle_timeout_ms: u64,

    /// TCP only: plugins do not apply to UDP redirects.
    #[serde(default)]
    pub minecraft: Option<MinecraftConfig>,

    /// UDP only: offline MOTD for a Bedrock relay. Ignored on TCP redirects.
    #[serde(default)]
    pub bedrock: Option<BedrockConfig>,

    /// TCP only: bandwidth cap in bits/second. Unset means unlimited.
    /// Ignored on UDP redirects.
    #[serde(default)]
    pub max_bandwidth_bps: Option<u64>,

    /// TCP only: which direction(s) `max_bandwidth_bps`/`random_wait_ms`
    /// apply to. Ignored on UDP redirects.
    #[serde(default)]
    pub wait_in_out: WaitInOut,

    /// TCP only: upper bound (milliseconds) of a random delay applied per
    /// chunk, in the direction(s) selected by `wait_in_out`. Unset means no
    /// jitter. Ignored on UDP redirects.
    #[serde(default)]
    pub random_wait_ms: Option<u64>,

    /// TCP only: read/write chunk size shaping is applied at. Ignored on
    /// UDP redirects, and unused when neither `max_bandwidth_bps` nor
    /// `random_wait_ms` is set.
    #[serde(default = "default_bufsize_bytes")]
    pub bufsize_bytes: usize,
}

#[derive(Debug, Deserialize)]
pub struct TargetConfig {
    #[serde(default)]
    target: Option<SocketAddr>,
    #[serde(default)]
    targets: Option<Vec<SocketAddr>>,
}

impl TargetConfig {
    pub fn from_ordered(targets: Vec<SocketAddr>) -> Self {
        assert!(
            !targets.is_empty(),
            "ordered targets must contain at least one address"
        );
        Self {
            target: None,
            targets: Some(targets),
        }
    }

    pub fn as_slice(&self) -> &[SocketAddr] {
        match (&self.target, &self.targets) {
            (Some(target), _) => std::slice::from_ref(target),
            (None, Some(targets)) => targets,
            (None, None) => &[],
        }
    }
}

impl RedirectConfig {
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            let targets = self.target_addrs();
            if targets.len() == 1 {
                format!("{} -> {}", self.listen, targets[0])
            } else {
                let targets = targets
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{} -> [{targets}]", self.listen)
            }
        })
    }

    pub fn target_addrs(&self) -> &[SocketAddr] {
        self.target_config.as_slice()
    }

    pub fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms)
    }

    pub fn udp_idle_timeout(&self) -> Duration {
        Duration::from_millis(self.udp_idle_timeout_ms)
    }

    pub fn minecraft_plugins(&self) -> Option<&MinecraftPluginsConfig> {
        self.minecraft.as_ref()?.plugins.as_ref()
    }

    pub fn bedrock_plugins(&self) -> Option<&BedrockPluginsConfig> {
        self.bedrock.as_ref()?.plugins.as_ref()
    }

    /// `None` when none of the shaping fields differ from their defaults,
    /// so callers can skip shaping entirely (including the child-process
    /// arg-passing on Unix) for the common case of an unshaped redirect.
    /// A custom `bufsize_bytes` alone is enough to return `Some` here, even
    /// with no bandwidth cap or jitter configured -- it still controls the
    /// copy loop's chunk size (see `shaping::shaped_copy_blocking`/`_async`).
    pub fn shaping(&self) -> Option<ShapingConfig> {
        if self.max_bandwidth_bps.is_none()
            && self.random_wait_ms.is_none()
            && self.bufsize_bytes == default_bufsize_bytes()
        {
            return None;
        }
        Some(ShapingConfig {
            max_bandwidth_bps: self.max_bandwidth_bps,
            wait_in_out: self.wait_in_out,
            random_wait_ms: self.random_wait_ms,
            bufsize: self.bufsize_bytes,
        })
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

/// Minecraft-specific settings for a redirect, namespaced so future
/// protocol-specific plugins can live alongside `plugins` without cluttering
/// the top-level redirect table.
#[derive(Debug, Deserialize)]
pub struct MinecraftConfig {
    #[serde(default)]
    pub plugins: Option<MinecraftPluginsConfig>,
}

#[derive(Debug, Deserialize)]
pub struct MinecraftPluginsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Text shown top-right in the server list in place of the player count
    /// (the SLP `version.name` field, paired with `protocol: -1`).
    #[serde(default)]
    pub status_line: Option<String>,

    /// First MOTD line, shown under the server name.
    #[serde(default)]
    pub motd_line1: Option<String>,

    /// Second MOTD line, shown under the server name.
    #[serde(default)]
    pub motd_line2: Option<String>,

    /// Path to a custom 64x64 PNG favicon. When unset, a built-in "offline
    /// scroll" icon is used.
    #[serde(default)]
    pub favicon_path: Option<std::path::PathBuf>,
}

/// Bedrock-specific settings for a UDP redirect, namespaced to mirror
/// `[redirect.minecraft]` for the TCP (Java edition) plugin.
#[derive(Debug, Deserialize)]
pub struct BedrockConfig {
    #[serde(default)]
    pub plugins: Option<BedrockPluginsConfig>,
}

#[derive(Debug, Deserialize)]
pub struct BedrockPluginsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// First MOTD line, shown as the server name in the Bedrock server list.
    #[serde(default)]
    pub motd_line1: Option<String>,

    /// Second MOTD line, shown as the sub-line (normally the world name).
    #[serde(default)]
    pub motd_line2: Option<String>,
}

fn default_connect_timeout_ms() -> u64 {
    3000
}

fn default_udp_idle_timeout_ms() -> u64 {
    60_000
}

fn default_true() -> bool {
    true
}

fn default_bufsize_bytes() -> usize {
    16 * 1024
}

impl FileConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path)
            .map_err(|err| ConfigError::Read(path.display().to_string(), err))?;
        let config: FileConfig = toml::from_str(&contents)
            .map_err(|err| ConfigError::Parse(path.display().to_string(), err))?;
        if config.redirects.is_empty() {
            return Err(ConfigError::Empty(path.display().to_string()));
        }
        for redirect in &config.redirects {
            let target_error = match (
                &redirect.target_config.target,
                &redirect.target_config.targets,
            ) {
                (Some(_), None) => None,
                (None, Some(targets)) if !targets.is_empty() => None,
                (None, Some(_)) => Some("`targets` must contain at least one address"),
                _ => Some("exactly one of `target` or `targets` must be set"),
            };
            if let Some(reason) = target_error {
                let redirect_name = redirect
                    .name
                    .clone()
                    .unwrap_or_else(|| redirect.listen.to_string());
                return Err(ConfigError::InvalidTargets(
                    redirect_name,
                    reason.to_owned(),
                ));
            }
        }
        Ok(config)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {0}: {1}")]
    Read(String, std::io::Error),
    #[error("failed to parse config file {0}: {1}")]
    Parse(String, toml::de::Error),
    #[error("config file {0} defines no [[redirect]] entries")]
    Empty(String),
    #[error("redirect {0} has invalid targets: {1}")]
    InvalidTargets(String, String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ordered_targets() {
        let config: FileConfig = toml::from_str(
            r#"
            [[redirect]]
            listen = "0.0.0.0:25565"
            targets = ["127.0.0.1:25566", "127.0.0.1:25567"]
            "#,
        )
        .unwrap();

        assert_eq!(
            config.redirects[0].target_addrs(),
            [
                "127.0.0.1:25566".parse::<SocketAddr>().unwrap(),
                "127.0.0.1:25567".parse::<SocketAddr>().unwrap(),
            ]
        );
    }

    #[test]
    fn normalizes_legacy_target_to_one_address() {
        let config: FileConfig = toml::from_str(
            r#"
            [[redirect]]
            listen = "0.0.0.0:25565"
            target = "127.0.0.1:25566"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.redirects[0].target_addrs(),
            ["127.0.0.1:25566".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn builds_target_config_from_ordered_addresses() {
        let targets = vec![
            "127.0.0.1:25566".parse::<SocketAddr>().unwrap(),
            "127.0.0.1:25567".parse::<SocketAddr>().unwrap(),
        ];

        let target_config = TargetConfig::from_ordered(targets.clone());

        assert_eq!(target_config.as_slice(), targets);
    }

    #[test]
    #[should_panic(expected = "ordered targets must contain at least one address")]
    fn rejects_empty_programmatic_target_config() {
        TargetConfig::from_ordered(Vec::new());
    }

    #[test]
    fn unnamed_multi_target_display_name_preserves_order() {
        let config: FileConfig = toml::from_str(
            r#"
            [[redirect]]
            listen = "0.0.0.0:25565"
            targets = ["127.0.0.1:25566", "127.0.0.1:25567"]
            "#,
        )
        .unwrap();

        assert_eq!(
            config.redirects[0].display_name(),
            "0.0.0.0:25565 -> [127.0.0.1:25566, 127.0.0.1:25567]"
        );
    }

    #[test]
    fn parses_minimal_redirect_with_defaults() {
        let config: FileConfig = toml::from_str(
            r#"
            [[redirect]]
            listen = "0.0.0.0:25565"
            target = "127.0.0.1:25566"
            "#,
        )
        .unwrap();

        assert_eq!(config.redirects.len(), 1);
        let r = &config.redirects[0];
        assert_eq!(r.name, None);
        assert_eq!(r.protocol, Protocol::Tcp);
        assert_eq!(r.connect_timeout_ms, 3000);
        assert_eq!(r.udp_idle_timeout_ms, 60_000);
        assert!(r.minecraft_plugins().is_none());
        assert_eq!(r.display_name(), "0.0.0.0:25565 -> 127.0.0.1:25566");

        assert_eq!(r.max_bandwidth_bps, None);
        assert_eq!(r.wait_in_out, WaitInOut::Both);
        assert_eq!(r.random_wait_ms, None);
        assert_eq!(r.bufsize_bytes, 16 * 1024);
        assert!(
            r.shaping().is_none(),
            "no shaping fields set -> no ShapingConfig"
        );
    }

    #[test]
    fn parses_shaping_fields_and_builds_shaping_config() {
        let config: FileConfig = toml::from_str(
            r#"
            [[redirect]]
            listen = "0.0.0.0:25565"
            target = "127.0.0.1:25566"
            max_bandwidth_bps = 1000000
            wait_in_out = "out"
            random_wait_ms = 50
            bufsize_bytes = 4096
            "#,
        )
        .unwrap();

        let r = &config.redirects[0];
        assert_eq!(r.max_bandwidth_bps, Some(1_000_000));
        assert_eq!(r.wait_in_out, WaitInOut::Out);
        assert_eq!(r.random_wait_ms, Some(50));
        assert_eq!(r.bufsize_bytes, 4096);

        let shaping = r
            .shaping()
            .expect("shaping fields set -> ShapingConfig present");
        assert_eq!(shaping.max_bandwidth_bps, Some(1_000_000));
        assert_eq!(shaping.wait_in_out, WaitInOut::Out);
        assert_eq!(shaping.random_wait_ms, Some(50));
        assert_eq!(shaping.bufsize, 4096);
    }

    /// A custom `bufsize_bytes` alone, with no bandwidth cap or jitter, must
    /// still produce a `ShapingConfig` -- otherwise it silently has no
    /// effect, which is exactly the surprise this test guards against.
    #[test]
    fn bufsize_alone_still_activates_shaping() {
        let config: FileConfig = toml::from_str(
            r#"
            [[redirect]]
            listen = "0.0.0.0:25565"
            target = "127.0.0.1:25566"
            bufsize_bytes = 32767
            "#,
        )
        .unwrap();

        let r = &config.redirects[0];
        let shaping = r
            .shaping()
            .expect("custom bufsize alone -> ShapingConfig present");
        assert_eq!(shaping.max_bandwidth_bps, None);
        assert_eq!(shaping.random_wait_ms, None);
        assert_eq!(shaping.bufsize, 32767);
    }

    #[test]
    fn parses_full_redirect_with_overrides_and_plugin() {
        let config: FileConfig = toml::from_str(
            r#"
            [[redirect]]
            name = "mc-main"
            listen = "0.0.0.0:25565"
            target = "127.0.0.1:25566"
            protocol = "udp"
            connect_timeout_ms = 1500
            udp_idle_timeout_ms = 30000

            [redirect.minecraft.plugins]
            enabled = true
            status_line = "custom status"
            motd_line1 = "line1"
            motd_line2 = "line2"
            "#,
        )
        .unwrap();

        let r = &config.redirects[0];
        assert_eq!(r.display_name(), "mc-main");
        assert_eq!(r.protocol, Protocol::Udp);
        assert_eq!(r.connect_timeout(), Duration::from_millis(1500));
        assert_eq!(r.udp_idle_timeout(), Duration::from_millis(30_000));

        let plugin = r.minecraft_plugins().unwrap();
        assert!(plugin.enabled);
        assert_eq!(plugin.status_line.as_deref(), Some("custom status"));
        assert_eq!(plugin.motd_line1.as_deref(), Some("line1"));
        assert_eq!(plugin.motd_line2.as_deref(), Some("line2"));
    }

    #[test]
    fn rejects_config_with_no_redirects() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("redir-rust-test-empty-{}.toml", std::process::id()));
        std::fs::write(&path, "").unwrap();

        let err = FileConfig::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Empty(_)));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_rejects_redirect_with_both_target_forms() {
        let path = std::env::temp_dir().join(format!(
            "redir-rust-test-both-target-forms-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
            [[redirect]]
            name = "mc-main"
            listen = "0.0.0.0:25565"
            target = "127.0.0.1:25566"
            targets = ["127.0.0.1:25567"]
            "#,
        )
        .unwrap();

        let err = FileConfig::load(&path).unwrap_err();
        match err {
            ConfigError::InvalidTargets(redirect, reason) => {
                assert_eq!(redirect, "mc-main");
                assert!(
                    reason.contains("exactly one"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_rejects_empty_targets() {
        let path = std::env::temp_dir().join(format!(
            "redir-rust-test-empty-targets-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
            [[redirect]]
            listen = "0.0.0.0:25565"
            targets = []
            "#,
        )
        .unwrap();

        let err = FileConfig::load(&path).unwrap_err();
        match err {
            ConfigError::InvalidTargets(redirect, reason) => {
                assert_eq!(redirect, "0.0.0.0:25565");
                assert!(
                    reason.contains("at least one"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_rejects_redirect_without_targets() {
        let path = std::env::temp_dir().join(format!(
            "redir-rust-test-missing-targets-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
            [[redirect]]
            name = "missing-target"
            listen = "0.0.0.0:25565"
            "#,
        )
        .unwrap();

        let err = FileConfig::load(&path).unwrap_err();
        match err {
            ConfigError::InvalidTargets(redirect, reason) => {
                assert_eq!(redirect, "missing-target");
                assert!(
                    reason.contains("exactly one"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_missing_file() {
        let err = FileConfig::load("/nonexistent/path/redir-rust-does-not-exist.toml").unwrap_err();
        assert!(matches!(err, ConfigError::Read(_, _)));
    }

    #[test]
    fn rejects_malformed_toml() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("redir-rust-test-bad-{}.toml", std::process::id()));
        std::fs::write(&path, "this is not valid toml [[[").unwrap();

        let err = FileConfig::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_, _)));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn loads_valid_file_end_to_end() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("redir-rust-test-valid-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
            [[redirect]]
            listen = "0.0.0.0:8080"
            target = "127.0.0.1:8081"
            "#,
        )
        .unwrap();

        let config = FileConfig::load(&path).unwrap();
        assert_eq!(config.redirects.len(), 1);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn loads_ordered_targets_end_to_end() {
        let path = std::env::temp_dir().join(format!(
            "redir-rust-test-valid-ordered-targets-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
            [[redirect]]
            listen = "0.0.0.0:25565"
            targets = ["127.0.0.1:25566", "127.0.0.1:25567"]
            "#,
        )
        .unwrap();

        let config = FileConfig::load(&path).unwrap();
        let redirect = &config.redirects[0];
        assert_eq!(
            redirect.target_addrs(),
            [
                "127.0.0.1:25566".parse::<SocketAddr>().unwrap(),
                "127.0.0.1:25567".parse::<SocketAddr>().unwrap(),
            ]
        );
        assert_eq!(
            redirect.display_name(),
            "0.0.0.0:25565 -> [127.0.0.1:25566, 127.0.0.1:25567]"
        );

        std::fs::remove_file(&path).ok();
    }
}
