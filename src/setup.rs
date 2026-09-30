//! Interactive setup for one named redirect, backed by the same config writer as --add.

use std::error::Error;
use std::io::{self, BufRead, IsTerminal, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, queue};

use redir_rust::config::{
    BedrockConfig, BedrockPluginsConfig, MinecraftConfig, MinecraftPluginsConfig, Protocol,
    RedirectConfig, TargetConfig,
};
use redir_rust::config_manager;
use redir_rust::shaping::WaitInOut;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub fn run(path: &Path) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    run_with(&mut stdin.lock(), &mut stdout.lock(), path)
}

fn run_with<R: BufRead, W: Write>(input: &mut R, output: &mut W, path: &Path) -> Result<()> {
    writeln!(output, "\n╭────────────────────────────────────────╮")?;
    writeln!(output, "│       redir-rust  ·  Setup Config       │")?;
    writeln!(output, "╰────────────────────────────────────────╯")?;
    section(output, 1, "Service")?;
    let name = loop {
        let value = required(input, output, "Service name")?;
        if config_manager::name_exists(path, &value)? {
            writeln!(
                output,
                "A redirect named {value:?} already exists. Choose another name."
            )?;
        } else {
            break value;
        }
    };
    let protocol = loop {
        match line(input, output, "Protocol [tcp/udp, default tcp]")?
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "tcp" => break Protocol::Tcp,
            "udp" => break Protocol::Udp,
            _ => writeln!(output, "Enter tcp or udp.")?,
        }
    };

    section(output, 2, "Public address")?;
    writeln!(output, "Public address clients will connect to:")?;
    let listen = socket(input, output, "Listen IP address", "Listen port")?;
    section(output, 3, "Backend targets")?;
    writeln!(output, "Targets are tried in the order shown below.")?;
    let mut targets = Vec::new();
    loop {
        let label = if targets.is_empty() {
            "Primary backend"
        } else {
            "Failover backend"
        };
        writeln!(output, "{label}:")?;
        targets.push(socket(input, output, "Backend IP address", "Backend port")?);
        if !yes_no(input, output, "Add another backend target? [y/N]", false)? {
            break;
        }
    }
    if protocol == Protocol::Udp && targets.len() > 1 {
        writeln!(
            output,
            "Multi-target UDP uses Bedrock RakNet probes to select a backend."
        )?;
    }

    section(output, 4, "Connection settings")?;
    let (connect_timeout_ms, udp_idle_timeout_ms) = match protocol {
        Protocol::Tcp => (
            positive_or_default(input, output, "Connect timeout in ms [3000]", 3000)?,
            60_000,
        ),
        Protocol::Udp => (
            3000,
            positive_or_default(input, output, "UDP idle timeout in ms [60000]", 60_000)?,
        ),
    };

    let plugin_label = match protocol {
        Protocol::Tcp => "Minecraft Java offline MOTD",
        Protocol::Udp => "Bedrock offline MOTD",
    };
    section(output, 5, "Plugins")?;
    let plugin_enabled = checkbox(input, output, plugin_label)?;
    let mut minecraft = None;
    let mut bedrock = None;
    if plugin_enabled {
        match protocol {
            Protocol::Tcp => {
                writeln!(
                    output,
                    "Plugin text (press Enter for each built-in default):"
                )?;
                let status_line = optional(input, output, "Status line")?;
                let motd_line1 = optional(input, output, "MOTD line 1")?;
                let motd_line2 = optional(input, output, "MOTD line 2")?;
                let favicon_path = loop {
                    let value = line(input, output, "Favicon PNG absolute path [built-in]")?;
                    if value.is_empty() {
                        break None;
                    }
                    let path = PathBuf::from(&value);
                    if path.is_absolute() && path.is_file() {
                        break Some(path);
                    }
                    writeln!(output, "Enter an existing absolute file path, or press Enter for the built-in icon.")?;
                };
                minecraft = Some(MinecraftConfig {
                    plugins: Some(MinecraftPluginsConfig {
                        enabled: true,
                        status_line,
                        motd_line1,
                        motd_line2,
                        favicon_path,
                    }),
                });
            }
            Protocol::Udp => {
                writeln!(
                    output,
                    "Plugin text (press Enter for each built-in default):"
                )?;
                bedrock = Some(BedrockConfig {
                    plugins: Some(BedrockPluginsConfig {
                        enabled: true,
                        motd_line1: optional(input, output, "MOTD line 1")?,
                        motd_line2: optional(input, output, "MOTD line 2")?,
                    }),
                });
            }
        }
    }

    section(output, 6, "Review and save")?;
    writeln!(output, "  Service       {name}")?;
    writeln!(
        output,
        "  Protocol      {}",
        if protocol == Protocol::Tcp {
            "tcp"
        } else {
            "udp"
        }
    )?;
    writeln!(output, "  Public listen {listen}")?;
    for (index, target) in targets.iter().enumerate() {
        writeln!(
            output,
            "  {} target: {target}",
            if index == 0 { "Primary" } else { "Failover" }
        )?;
    }
    writeln!(
        output,
        "  Plugin        {}",
        if plugin_enabled { plugin_label } else { "none" }
    )?;
    writeln!(output, "  Config file   {}", path.display())?;
    if !yes_no(input, output, "Save this redirect? [Y/n]", true)? {
        writeln!(output, "Cancelled; config unchanged.")?;
        return Ok(());
    }

    config_manager::add(
        path,
        RedirectConfig {
            name: Some(name),
            listen,
            target_config: TargetConfig::from_ordered(targets),
            protocol,
            connect_timeout_ms,
            udp_idle_timeout_ms,
            minecraft,
            bedrock,
            max_bandwidth_bps: None,
            wait_in_out: WaitInOut::Both,
            random_wait_ms: None,
            bufsize_bytes: 16 * 1024,
        },
    )?;
    writeln!(output, "Saved. Restart redir-rust to apply the new config.")?;
    Ok(())
}

/// Guided editing from the terminal manager; raw CLI editing remains available.
pub fn edit(path: &Path) -> Result<()> {
    edit_with(&mut io::stdin().lock(), &mut io::stdout().lock(), path)
}

fn edit_with<R: BufRead, W: Write>(input: &mut R, output: &mut W, path: &Path) -> Result<()> {
    loop {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err.into()),
        };
        let config: redir_rust::config::FileConfig = toml::from_str(&text)?;
        if config.redirects.is_empty() {
            writeln!(output, "No services configured. Use ➕ Setup Config first.")?;
            return Ok(());
        }
        redir_rust::config::FileConfig::parse_str(&text)?;
        let mut services: Vec<_> = config
            .redirects
            .iter()
            .map(|r| {
                format!(
                    "🎮 {}",
                    r.name
                        .clone()
                        .unwrap_or_else(|| format!("Unnamed ({})", r.listen))
                )
            })
            .collect();
        services.push("⬅ Previous".into());
        let Some(index) = edit_choice(
            input,
            output,
            "📝 Which service do you want to edit?",
            &services,
        )?
        else {
            return Ok(());
        };
        if index == config.redirects.len() {
            return Ok(());
        }
        loop {
            let mut config = redir_rust::config::FileConfig::load(path)?;
            let redirect = config.redirects.remove(index);
            let fields = vec![
                format!("📡 Listen address: {}", redirect.listen),
                format!(
                    "🎯 Destinations: {}",
                    redirect
                        .target_config
                        .as_slice()
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                format!(
                    "🏷 Service name: {}",
                    redirect.name.as_deref().unwrap_or("Unnamed")
                ),
                format!(
                    "🔌 Protocol: {}",
                    if redirect.protocol == Protocol::Tcp {
                        "tcp"
                    } else {
                        "udp"
                    }
                ),
                format!(
                    "⏱ Timeout: {} ms",
                    if redirect.protocol == Protocol::Tcp {
                        redirect.connect_timeout_ms
                    } else {
                        redirect.udp_idle_timeout_ms
                    }
                ),
                "🛠 Advanced editor (plugins and other settings)".into(),
                "⬅ Previous".into(),
            ];
            let Some(field) = edit_choice(input, output, "⚙ What do you want to edit?", &fields)?
            else {
                break;
            };
            if field == fields.len() - 1 {
                break;
            }
            if field == 5 {
                if let Some(name) = redirect.name.as_deref() {
                    config_manager::edit(path, name)?;
                } else {
                    redir_rust::edit_config::run(path);
                }
                writeln!(output, "Restart redir-rust to apply saved changes.")?;
                break;
            }
            match edit_field(input, output, path, index, field, redirect) {
                Ok(()) => {}
                Err(err)
                    if err
                        .downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::Interrupted) =>
                {
                    writeln!(output, "⬅ Returned without saving.")?;
                }
                Err(err)
                    if err
                        .downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::UnexpectedEof) =>
                {
                    return Ok(())
                }
                Err(err) => writeln!(output, "Could not save: {err}")?,
            }
        }
    }
}

fn edit_choice<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    title: &str,
    items: &[String],
) -> io::Result<Option<usize>> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        let labels: Vec<_> = items.iter().map(String::as_str).collect();
        return crate::menu::select_with_arrows(title, &labels);
    }
    loop {
        writeln!(output, "\n{title}")?;
        for (index, item) in items[..items.len() - 1].iter().enumerate() {
            writeln!(output, "  {}  {item}", index + 1)?;
        }
        writeln!(output, "  0  ⬅ Previous")?;
        let answer = match line(input, output, "Choice") {
            Ok(answer) => answer,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(err),
        };
        match answer.as_str() {
            "0" | "back" => return Ok(None),
            _ => match answer.parse::<usize>() {
                Ok(n) if n > 0 && n < items.len() => return Ok(Some(n - 1)),
                _ => writeln!(output, "Choose 1-{}, or 0 for ⬅ Previous.", items.len() - 1)?,
            },
        }
    }
}

fn edit_value<R: BufRead, W: Write, T: std::str::FromStr + std::fmt::Display>(
    input: &mut R,
    output: &mut W,
    label: &str,
    current: T,
    valid: impl Fn(&T) -> bool,
) -> io::Result<T> {
    loop {
        let value = line(
            input,
            output,
            &format!("{label} [{current}] (back = ⬅ Previous)"),
        )?;
        if value.eq_ignore_ascii_case("back") {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "previous"));
        }
        if value.is_empty() {
            return Ok(current);
        }
        match value.parse::<T>() {
            Ok(value) if valid(&value) => return Ok(value),
            _ => writeln!(output, "Enter a valid {label}, or type back.")?,
        }
    }
}

fn edit_socket<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    current: SocketAddr,
) -> io::Result<SocketAddr> {
    let ip = edit_value(input, output, "IP address", current.ip(), |_| true)?;
    let port = edit_value(input, output, "Port (1-65535)", current.port(), |p| *p > 0)?;
    Ok(SocketAddr::new(ip, port))
}

fn edit_field<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    path: &Path,
    index: usize,
    field: usize,
    mut redirect: RedirectConfig,
) -> Result<()> {
    match field {
        0 => redirect.listen = edit_socket(input, output, redirect.listen)?,
        1 => {
            let mut targets = Vec::new();
            loop {
                let current = redirect
                    .target_config
                    .as_slice()
                    .get(targets.len())
                    .copied()
                    .unwrap_or_else(|| redirect.target_config.as_slice()[0]);
                writeln!(
                    output,
                    "🎯 Destination {} (priority order)",
                    targets.len() + 1
                )?;
                targets.push(edit_socket(input, output, current)?);
                let more = edit_value(
                    input,
                    output,
                    "Add another destination? y/n",
                    "n".to_string(),
                    |s| matches!(s.as_str(), "y" | "n"),
                )?;
                if more == "n" {
                    break;
                }
            }
            redirect.target_config = TargetConfig::from_ordered(targets);
        }
        2 => {
            redirect.name = Some(edit_value(
                input,
                output,
                "Service name",
                redirect.name.take().unwrap_or_default(),
                |s: &String| !s.trim().is_empty(),
            )?)
        }
        3 => {
            let current = if redirect.protocol == Protocol::Tcp {
                "tcp"
            } else {
                "udp"
            };
            let value = edit_value(
                input,
                output,
                "Protocol tcp/udp",
                current.to_string(),
                |s| matches!(s.as_str(), "tcp" | "udp"),
            )?;
            redirect.protocol = if value == "tcp" {
                Protocol::Tcp
            } else {
                Protocol::Udp
            };
        }
        4 => {
            let value = if redirect.protocol == Protocol::Tcp {
                &mut redirect.connect_timeout_ms
            } else {
                &mut redirect.udp_idle_timeout_ms
            };
            *value = edit_value(input, output, "Timeout in ms", *value, |v| *v > 0)?;
        }
        _ => unreachable!(),
    }
    if redirect.protocol == Protocol::Udp && redirect.target_config.as_slice().len() > 1 {
        writeln!(output, "Multi-target UDP uses Bedrock RakNet probes.")?;
    }
    writeln!(
        output,
        "\n📋 Review service: {}",
        redirect.name.as_deref().unwrap_or("Unnamed")
    )?;
    writeln!(output, "{}", toml::to_string(&redirect)?)?;
    let save = edit_value(
        input,
        output,
        "💾 Save changes? y/n",
        "y".to_string(),
        |s| matches!(s.as_str(), "y" | "n"),
    )?;
    if save == "y" {
        config_manager::update(path, index, redirect)?;
        writeln!(output, "✅ Saved. Restart redir-rust to apply changes.")?;
    } else {
        writeln!(output, "Cancelled; config unchanged.")?;
    }
    Ok(())
}

fn section<W: Write>(output: &mut W, number: usize, title: &str) -> io::Result<()> {
    writeln!(output, "\n  ── {number}/6  {title} ──")
}

fn line<R: BufRead, W: Write>(input: &mut R, output: &mut W, label: &str) -> io::Result<String> {
    write!(output, "{label}: ")?;
    output.flush()?;
    let mut value = String::new();
    if input.read_line(&mut value)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "setup input ended",
        ));
    }
    Ok(value.trim().to_string())
}

fn required<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    label: &str,
) -> io::Result<String> {
    loop {
        let value = line(input, output, label)?;
        if !value.is_empty() {
            return Ok(value);
        }
        writeln!(output, "{label} is required.")?;
    }
}

fn optional<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    label: &str,
) -> io::Result<Option<String>> {
    Ok(Some(line(input, output, label)?).filter(|value| !value.is_empty()))
}

fn socket<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    host_label: &str,
    port_label: &str,
) -> io::Result<SocketAddr> {
    let ip = loop {
        let value = required(input, output, host_label)?;
        match value.parse::<IpAddr>() {
            Ok(ip) => break ip,
            Err(_) => writeln!(output, "Enter a valid IPv4 or IPv6 address.")?,
        }
    };
    let port = loop {
        let value = required(input, output, port_label)?;
        match value.parse::<u16>() {
            Ok(port) if port > 0 => break port,
            _ => writeln!(output, "Enter a port from 1 to 65535.")?,
        }
    };
    Ok(SocketAddr::new(ip, port))
}

fn positive_or_default<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    label: &str,
    default: u64,
) -> io::Result<u64> {
    loop {
        let value = line(input, output, label)?;
        if value.is_empty() {
            return Ok(default);
        }
        match value.parse::<u64>() {
            Ok(number) if number > 0 => return Ok(number),
            _ => writeln!(
                output,
                "Enter a positive number, or press Enter for the default."
            )?,
        }
    }
}

fn yes_no<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    label: &str,
    default: bool,
) -> io::Result<bool> {
    loop {
        match line(input, output, label)?.to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(output, "Enter y or n.")?,
        }
    }
}

fn checkbox<R: BufRead, W: Write>(input: &mut R, output: &mut W, label: &str) -> io::Result<bool> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        return tty_checkbox(output, label);
    }
    let mut selected = false;
    loop {
        writeln!(
            output,
            "\n  Select supported plugin (1 toggles, Enter continues)"
        )?;
        writeln!(
            output,
            "  1  [{}] {label}",
            if selected { 'x' } else { ' ' }
        )?;
        match line(input, output, "Choice")?.as_str() {
            "" => return Ok(selected),
            "1" => selected = !selected,
            _ => writeln!(
                output,
                "Enter 1 to toggle the checkbox, or press Enter to continue."
            )?,
        }
    }
}

struct RawMode;

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn tty_checkbox<W: Write>(output: &mut W, label: &str) -> io::Result<bool> {
    terminal::enable_raw_mode()?;
    let _raw = RawMode;
    let mut selected = false;
    let mut row = 0;
    let mut first = true;
    loop {
        if !first {
            queue!(
                output,
                cursor::MoveUp(3),
                terminal::Clear(ClearType::FromCursorDown)
            )?;
        }
        first = false;
        write!(output, "  ↑/↓ choose · Space toggle · Enter select\r\n")?;
        write!(
            output,
            "  {} [{}] {label}\r\n",
            if row == 0 { "▶" } else { " " },
            if selected { 'x' } else { ' ' }
        )?;
        write!(
            output,
            "  {} Continue\r\n",
            if row == 1 { "▶" } else { " " }
        )?;
        output.flush()?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Down => row = 1 - row,
            KeyCode::Char(' ') => selected = !selected,
            KeyCode::Enter if row == 0 => selected = !selected,
            KeyCode::Enter => return Ok(selected),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redir_rust::config::FileConfig;
    use std::fs;
    use std::io::Cursor;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "redir-rust-setup-{}-{unique}.toml",
            std::process::id()
        ))
    }

    fn edit_fixture(path: &Path) -> String {
        let original = "# Keep this header\n[[redirect]]\nname = \"service.java\"\nlisten = \"0.0.0.0:25565\"\ntarget = \"127.0.0.1:25566\"\n\n[[redirect]] # keep this block exactly\nname = \"service.bedrock\"\nlisten = \"0.0.0.0:19132\"\ntargets = [\"127.0.0.1:19133\", \"127.0.0.1:19134\"]\nprotocol = \"udp\"\n";
        fs::write(path, original).unwrap();
        original.to_string()
    }

    #[test]
    fn guided_edit_selects_service_and_changes_only_its_listen_address() {
        let path = test_path();
        let original = edit_fixture(&path);
        let mut output = Vec::new();
        // Second service, Listen, invalid IP/port then valid IPv6, save, back.
        let answers = "2\n1\ninvalid\n::1\n0\n19140\ny\n0\n0\n";
        edit_with(&mut Cursor::new(answers), &mut output, &path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        let config = FileConfig::parse_str(&saved).unwrap();
        assert_eq!(config.redirects[1].listen.to_string(), "[::1]:19140");
        assert_eq!(config.redirects[1].target_config.as_slice().len(), 2);
        assert!(saved.starts_with(original.split("[[redirect]] #").next().unwrap()));
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("service.java"));
        assert!(output.contains("service.bedrock"));
        assert!(output.contains("Destinations"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn guided_edit_saves_ordered_destinations() {
        let path = test_path();
        let original = edit_fixture(&path);
        let answers = "1\n2\n10.0.0.1\n25570\ny\n10.0.0.2\n25571\nn\ny\n0\n0\n";
        edit_with(&mut Cursor::new(answers), &mut Vec::new(), &path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        let config = FileConfig::parse_str(&saved).unwrap();
        let targets: Vec<_> = config.redirects[0]
            .target_config
            .as_slice()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(targets, ["10.0.0.1:25570", "10.0.0.2:25571"]);
        let other = &original[original.find("[[redirect]] #").unwrap()..];
        assert!(saved.ends_with(other));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn guided_edit_cancel_and_eof_preserve_original_config() {
        for answers in ["1\n1\n127.0.0.1\n25570\nn\n0\n0\n", "0\n", "1\n1\n"] {
            let path = test_path();
            let original = edit_fixture(&path);
            let _ = edit_with(&mut Cursor::new(answers), &mut Vec::new(), &path);
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn guided_edit_previous_returns_to_fields_then_service_selection() {
        let path = test_path();
        let original = edit_fixture(&path);
        // Back from a partially entered listen address, then choose the
        // other service and back from its destination before saving anything.
        let answers = "1\n1\n127.0.0.2\nback\n0\n2\n2\nback\n0\n0\n";
        let mut output = Vec::new();
        edit_with(&mut Cursor::new(answers), &mut output, &path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("Returned without saving").count(), 2);
        assert!(output.contains("⬅ Previous"));
        assert!(output.contains("🎯 Destinations"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn guided_edit_rejects_duplicate_name_then_allows_valid_rename_and_timeout() {
        let path = test_path();
        edit_fixture(&path);
        let answers =
            "1\n3\nservice.bedrock\ny\n3\nservice.renamed\ny\n4\nudp\ny\n5\n0\n90000\ny\n0\n0\n";
        let mut output = Vec::new();
        edit_with(&mut Cursor::new(answers), &mut output, &path).unwrap();
        let config = FileConfig::load(&path).unwrap();
        assert_eq!(config.redirects[0].name.as_deref(), Some("service.renamed"));
        assert_eq!(config.redirects[0].protocol, Protocol::Udp);
        assert_eq!(config.redirects[0].udp_idle_timeout_ms, 90_000);
        assert_eq!(config.redirects[0].connect_timeout_ms, 3_000);
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("Could not save"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn guided_edit_empty_config_has_no_services() {
        let path = test_path();
        fs::write(&path, redir_rust::config::DEFAULT_CONFIG_TOML).unwrap();
        let mut output = Vec::new();
        edit_with(&mut Cursor::new(""), &mut output, &path).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("No services"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn saves_ordered_bedrock_targets_and_selected_plugin() {
        let path = test_path();
        let answers = "bedrock-relay\nudp\n0.0.0.0\n19132\n10.0.0.20\n19133\ny\n10.0.0.21\n19133\nn\n\n1\n\nOffline\nTry later\ny\n";
        let mut output = Vec::new();
        run_with(&mut Cursor::new(answers), &mut output, &path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        let config = FileConfig::parse_str(&saved).unwrap();
        let redirect = &config.redirects[0];
        assert_eq!(redirect.name.as_deref(), Some("bedrock-relay"));
        assert_eq!(redirect.listen.to_string(), "0.0.0.0:19132");
        assert_eq!(
            redirect
                .target_config
                .as_slice()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["10.0.0.20:19133", "10.0.0.21:19133"]
        );
        assert_eq!(
            redirect
                .bedrock
                .as_ref()
                .unwrap()
                .plugins
                .as_ref()
                .unwrap()
                .motd_line1
                .as_deref(),
            Some("Offline")
        );
        assert!(saved.contains("targets = ["));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn duplicate_name_is_reprompted_before_other_questions() {
        let path = test_path();
        let answers = "relay\ntcp\n127.0.0.1\n25565\n127.0.0.1\n25566\nn\n\n\ny\n";
        run_with(&mut Cursor::new(answers), &mut Vec::new(), &path).unwrap();
        let answers = "relay\nrelay-two\ntcp\n127.0.0.1\n25567\n127.0.0.1\n25568\nn\n\n\ny\n";
        let mut output = Vec::new();
        run_with(&mut Cursor::new(answers), &mut output, &path).unwrap();
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("already exists"));
        let config = FileConfig::parse_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.redirects.len(), 2);
        assert_eq!(config.redirects[1].name.as_deref(), Some("relay-two"));
        fs::remove_file(path).unwrap();
    }
}
