//! Interactive setup for one named redirect, backed by the same config writer as --add.

use std::error::Error;
use std::io::{self, BufRead, IsTerminal, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

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
    match run_with(&mut stdin.lock(), &mut stdout.lock(), path) {
        Err(err) if is_cancelled(err.as_ref()) => {
            if interactive() {
                cliclack::outro_cancel("Returned without saving")?;
            }
            Ok(())
        }
        result => result,
    }
}

fn run_with<R: BufRead, W: Write>(input: &mut R, output: &mut W, path: &Path) -> Result<()> {
    if interactive() {
        cliclack::clear_screen()?;
        cliclack::intro("redir-rust · Setup Config")?;
    } else {
        writeln!(output, "\n╭────────────────────────────────────────╮")?;
        writeln!(output, "│       redir-rust  ·  Setup Config       │")?;
        writeln!(output, "╰────────────────────────────────────────╯")?;
    }
    section(output, 1, "Service")?;
    let name = loop {
        let value = required(input, output, "Service name")?;
        if config_manager::name_exists(path, &value)? {
            message(
                output,
                &format!("A redirect named {value:?} already exists. Choose another name."),
            )?;
        } else {
            break value;
        }
    };
    let protocol = if interactive() {
        match crate::prompts::select("Protocol")?
            .item("tcp", "TCP", "")
            .item("udp", "UDP", "")
            .interact()?
        {
            "udp" => Protocol::Udp,
            _ => Protocol::Tcp,
        }
    } else {
        loop {
            match line(input, output, "Protocol [tcp/udp, default tcp]")?
                .to_ascii_lowercase()
                .as_str()
            {
                "" | "tcp" => break Protocol::Tcp,
                "udp" => break Protocol::Udp,
                _ => writeln!(output, "Enter tcp or udp.")?,
            }
        }
    };

    section(output, 2, "Listen address")?;
    message(output, "Accept client connections on this machine:")?;
    let listen = loop {
        let addr = socket(input, output, "Listen IP address", "Listen port", true)?;
        let available = match protocol {
            Protocol::Tcp => std::net::TcpListener::bind(addr).map(drop),
            Protocol::Udp => std::net::UdpSocket::bind(addr).map(drop),
        };
        match available {
            Ok(()) => break addr,
            Err(err) => {
                let protocol_name = if protocol == Protocol::Tcp {
                    "TCP"
                } else {
                    "UDP"
                };
                let reason = if err.kind() == io::ErrorKind::AddrInUse {
                    "already in use".to_string()
                } else {
                    err.to_string()
                };
                let error = format!("Cannot listen on {protocol_name} {addr}: {reason}");
                if interactive() {
                    if crate::prompts::select(&error)?
                        .item("retry", "Choose another listen address or port", "")
                        .item("back", "⬅ Previous", "Return without saving")
                        .interact()?
                        == "back"
                    {
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "previous").into());
                    }
                } else {
                    message(
                        output,
                        &format!("{error}. Choose another listen address or port."),
                    )?;
                }
            }
        }
    };
    section(output, 3, "Backend targets")?;
    message(output, "Targets are tried in the order shown below.")?;
    let mut targets = Vec::new();
    loop {
        let label = if targets.is_empty() {
            "Primary backend"
        } else {
            "Failover backend"
        };
        message(output, label)?;
        targets.push(socket(
            input,
            output,
            "Backend IP address",
            "Backend port",
            false,
        )?);
        if !yes_no(
            input,
            output,
            "Add another backend target? [y/N]",
            false,
            None,
        )? {
            break;
        }
    }
    if protocol == Protocol::Udp && targets.len() > 1 {
        message(
            output,
            "Multi-target UDP uses Bedrock RakNet probes to select a backend.",
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
                message(
                    output,
                    "Plugin text (press Enter for each built-in default):",
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
                message(
                    output,
                    "Plugin text (press Enter for each built-in default):",
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
    let mut review = vec![
        format!("Service       {name}"),
        format!(
            "Protocol      {}",
            if protocol == Protocol::Tcp {
                "tcp"
            } else {
                "udp"
            }
        ),
        format!("Listen        {listen}"),
    ];
    for (index, target) in targets.iter().enumerate() {
        review.push(format!(
            "{} target  {target}",
            if index == 0 { "Primary" } else { "Failover" }
        ));
    }
    review.push(format!(
        "Plugin        {}",
        if plugin_enabled { plugin_label } else { "none" }
    ));
    review.push(format!("Config file   {}", path.display()));
    let review = review.join("\n");
    if !interactive() {
        writeln!(output, "{review}")?;
    }
    if !yes_no(
        input,
        output,
        "Save this redirect? [Y/n]",
        true,
        Some(("Review redirect", &review)),
    )? {
        if interactive() {
            cliclack::outro_cancel("Cancelled; config unchanged.")?;
        } else {
            writeln!(output, "Cancelled; config unchanged.")?;
        }
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
    if interactive() {
        cliclack::outro("Saved. Reload redir-rust to apply the new config.")?;
    } else {
        writeln!(output, "Saved. Reload redir-rust to apply the new config.")?;
    }
    Ok(())
}

/// Guided editing from the terminal manager; raw CLI editing remains available.
pub fn edit(path: &Path) -> Result<()> {
    if interactive() {
        cliclack::clear_screen()?;
        cliclack::intro("redir-rust · Edit Config")?;
    }
    match edit_with(&mut io::stdin().lock(), &mut io::stdout().lock(), path) {
        Err(err) if is_cancelled(err.as_ref()) => Ok(()),
        result => {
            if interactive() && result.is_ok() {
                cliclack::outro("Returned to the manager")?;
            }
            result
        }
    }
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
                writeln!(output, "Reload redir-rust to apply saved changes.")?;
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
    if interactive() {
        let mut prompt = crate::prompts::select(title)?;
        for (index, label) in items.iter().enumerate() {
            prompt = prompt.item(index, label, "");
        }
        return match prompt.interact() {
            Ok(index) => Ok((index < items.len() - 1).then_some(index)),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(err) => Err(err),
        };
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
    if interactive() {
        match crate::prompts::select(label)?
            .item("keep", format!("Keep {current}"), "Current value")
            .item("change", "Enter a new value", "")
            .item("back", "⬅ Previous", "Discard pending changes")
            .interact()?
        {
            "keep" => return Ok(current),
            "back" => return Err(io::Error::new(io::ErrorKind::Interrupted, "previous")),
            _ => {}
        }
        loop {
            let value: String = crate::prompts::input(label)?
                .default_input(&current.to_string())
                .interact()?;
            match value.trim().parse::<T>() {
                Ok(value) if valid(&value) => return Ok(value),
                _ => {
                    cliclack::log::warning(format!("Enter a valid {label}. Press Esc to return."))?
                }
            }
        }
    }
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

/// Discrete edit decisions include navigation as a selectable option.
fn edit_selection<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    label: &str,
    current: &str,
    options: &[(&str, &str)],
    review: Option<(&str, &str)>,
) -> io::Result<String> {
    if interactive() {
        let mut prompt =
            crate::prompts::select(label.trim_end_matches(" y/n"))?.initial_value(current);
        for (value, title) in options {
            prompt = prompt.item(*value, *title, "");
        }
        if let Some((title, text)) = review {
            cliclack::note(title, text)?;
        }
        let selected = prompt
            .item("back", "⬅ Previous", "Discard pending changes")
            .interact()?;
        if selected == "back" {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "previous"));
        }
        return Ok(selected.to_string());
    }
    edit_value(input, output, label, current.to_string(), |value| {
        options.iter().any(|(option, _)| value == option)
    })
}

fn edit_socket<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    current: SocketAddr,
    local: bool,
) -> io::Result<SocketAddr> {
    let ip = ip_address(input, output, "IP address", Some(current.ip()), local)?;
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
        0 => redirect.listen = edit_socket(input, output, redirect.listen, true)?,
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
                targets.push(edit_socket(input, output, current, false)?);
                let more = edit_selection(
                    input,
                    output,
                    "Add another destination? y/n",
                    "n",
                    &[
                        ("y", "Yes — add another destination"),
                        ("n", "No — continue to review"),
                    ],
                    None,
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
            let value = edit_selection(
                input,
                output,
                "Protocol tcp/udp",
                current,
                &[("tcp", "TCP"), ("udp", "UDP")],
                None,
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
    let title = format!(
        "Review service: {}",
        redirect.name.as_deref().unwrap_or("Unnamed")
    );
    let review = toml::to_string(&redirect)?;
    if !interactive() {
        writeln!(output, "\n📋 {title}\n{review}")?;
    }
    let save = edit_selection(
        input,
        output,
        "💾 Save changes? y/n",
        "y",
        &[("y", "Yes — save changes"), ("n", "No — discard changes")],
        Some((&title, &review)),
    )?;
    if save == "y" {
        config_manager::update(path, index, redirect)?;
        writeln!(output, "✅ Saved. Reload redir-rust to apply changes.")?;
    } else {
        writeln!(output, "Cancelled; config unchanged.")?;
    }
    Ok(())
}

fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn is_cancelled(err: &(dyn Error + 'static)) -> bool {
    err.downcast_ref::<io::Error>()
        .is_some_and(|err| err.kind() == io::ErrorKind::Interrupted)
}

fn message<W: Write>(output: &mut W, text: &str) -> io::Result<()> {
    if interactive() {
        cliclack::log::info(text)
    } else {
        writeln!(output, "{text}")
    }
}

fn section<W: Write>(output: &mut W, number: usize, title: &str) -> io::Result<()> {
    message(output, &format!("{number}/6 · {title}"))
}

fn line<R: BufRead, W: Write>(input: &mut R, output: &mut W, label: &str) -> io::Result<String> {
    if interactive() {
        let value: String = crate::prompts::input(label)?.required(false).interact()?;
        return Ok(value.trim().to_string());
    }
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
    if interactive() {
        return crate::prompts::input(label)?
            .validate(|value: &String| {
                if value.trim().is_empty() {
                    Err("Value is required")
                } else {
                    Ok(())
                }
            })
            .interact::<String>()
            .map(|value| value.trim().to_string());
    }
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
    local: bool,
) -> io::Result<SocketAddr> {
    let ip = ip_address(input, output, host_label, None, local)?;
    let port = if interactive() {
        crate::prompts::input(port_label)?
            .validate(|value: &String| {
                if value.parse::<u16>().is_ok_and(|port| port > 0) {
                    Ok(())
                } else {
                    Err("Enter a port from 1 to 65535")
                }
            })
            .interact::<u16>()?
    } else {
        loop {
            let value = required(input, output, port_label)?;
            match value.parse::<u16>() {
                Ok(port) if port > 0 => break port,
                _ => writeln!(output, "Enter a port from 1 to 65535.")?,
            }
        }
    };
    Ok(SocketAddr::new(ip, port))
}

fn ip_address<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    label: &str,
    current: Option<IpAddr>,
    local: bool,
) -> io::Result<IpAddr> {
    if interactive() {
        loop {
            let mut prompt = crate::prompts::select(label)?;
            if let Some(ip) = current {
                prompt = prompt.item("current", format!("Keep {ip}"), "Current address");
            }
            prompt = if local {
                prompt.item("any", "All IPv4 interfaces", "0.0.0.0")
            } else {
                prompt.item("tailscale", "Choose a Tailscale device", "Backend peers")
            };
            prompt = if local {
                prompt.item("tailscale", "This device's Tailscale address", "")
            } else {
                prompt.item("localhost", "Localhost", "127.0.0.1")
            };
            match prompt
                .item("manual", "Enter an IP address", "IPv4 or IPv6")
                .item("back", "⬅ Previous", "Discard pending input")
                .interact()?
            {
                "current" => return Ok(current.expect("current option requires an address")),
                "any" => return Ok(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
                "localhost" => return Ok(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
                "back" => return Err(io::Error::new(io::ErrorKind::Interrupted, "previous")),
                "tailscale" => {
                    if let Some(ip) = tailscale_address(input, output, local)? {
                        return Ok(ip);
                    }
                }
                _ => {
                    let value: IpAddr = crate::prompts::input("IP address")?
                        .placeholder("IPv4 or IPv6")
                        .interact()?;
                    return Ok(value);
                }
            }
        }
    }
    loop {
        let default = current.map(|ip| format!(" [{ip}]")).unwrap_or_default();
        let value = line(
            input,
            output,
            &format!("{label}{default} (tailscale = choose device, back = Previous)"),
        )?;
        if value.eq_ignore_ascii_case("back") {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "previous"));
        }
        if value.is_empty() {
            if let Some(ip) = current {
                return Ok(ip);
            }
        }
        if value.eq_ignore_ascii_case("tailscale") {
            if let Some(ip) = tailscale_address(input, output, local)? {
                return Ok(ip);
            }
        } else if let Ok(ip) = value.parse() {
            return Ok(ip);
        } else {
            writeln!(
                output,
                "Enter a valid IPv4 or IPv6 address, or type tailscale."
            )?;
        }
    }
}

fn tailscale_address<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    local: bool,
) -> io::Result<Option<IpAddr>> {
    let spinner = interactive().then(cliclack::spinner);
    if let Some(spinner) = &spinner {
        spinner.start("Fetching Tailscale devices");
    } else {
        writeln!(output, "Fetching Tailscale devices…")?;
    }
    let result = crate::tailscale::fetch(local);
    if let Some(spinner) = &spinner {
        spinner.stop("Tailscale lookup finished");
    }
    match result {
        Ok(devices) if !devices.is_empty() => choose_tailscale(input, output, &devices),
        Ok(_) => {
            message(
                output,
                "No Tailscale addresses available. Choose another address source or retry.",
            )?;
            Ok(None)
        }
        Err(err) => {
            message(
                output,
                &format!("Cannot fetch Tailscale devices: {err}. Enter an IP manually or retry."),
            )?;
            Ok(None)
        }
    }
}

fn choose_tailscale<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    devices: &[crate::tailscale::DeviceAddress],
) -> io::Result<Option<IpAddr>> {
    if interactive() {
        let mut prompt = crate::prompts::select("Choose a Tailscale device/address")?
            .filter_mode()
            .max_rows(10);
        for (index, device) in devices.iter().enumerate() {
            prompt = prompt.item(index, &device.label, "");
        }
        return match prompt
            .item(devices.len(), "Previous", "Address source")
            .interact()
        {
            Ok(index) => Ok(devices.get(index).map(|device| device.ip)),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(err) => Err(err),
        };
    }
    let mut labels: Vec<_> = devices.iter().map(|device| device.label.clone()).collect();
    labels.push("⬅ Previous (manual IP)".into());
    Ok(
        edit_choice(input, output, "Choose a Tailscale device/address", &labels)?
            .filter(|index| *index < devices.len())
            .map(|index| devices[index].ip),
    )
}

fn positive_or_default<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    label: &str,
    default: u64,
) -> io::Result<u64> {
    if interactive() {
        return crate::prompts::input(label)?
            .default_input(&default.to_string())
            .validate(|value: &String| {
                if value.parse::<u64>().is_ok_and(|number| number > 0) {
                    Ok(())
                } else {
                    Err("Enter a positive number")
                }
            })
            .interact();
    }
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
    review: Option<(&str, &str)>,
) -> io::Result<bool> {
    if interactive() {
        let title = label.trim_end_matches(" [y/N]").trim_end_matches(" [Y/n]");
        let mut selected = crate::prompts::select(title)?
            .item("yes", "Yes", "")
            .item("no", "No", "")
            .item("back", "⬅ Previous", "Return without saving")
            .initial_value(if default { "yes" } else { "no" });
        if let Some((title, text)) = review {
            cliclack::note(title, text)?;
        }
        match selected.interact()? {
            "yes" => return Ok(true),
            "no" => return Ok(false),
            _ => return Err(io::Error::new(io::ErrorKind::Interrupted, "previous")),
        }
    }
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
    if interactive() {
        return Ok(!crate::prompts::multiselect("Enable optional plugins")?
            .item(0, label, "Handles offline responses")
            .required(false)
            .interact()?
            .is_empty());
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
    fn setup_reports_occupied_tcp_and_udp_ports_without_saving() {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        for (protocol, addr) in [
            ("tcp", tcp.local_addr().unwrap()),
            ("udp", udp.local_addr().unwrap()),
        ] {
            let path = test_path();
            let answers = format!("relay\n{protocol}\n127.0.0.1\n{}\n", addr.port());
            let mut output = Vec::new();
            assert!(run_with(&mut Cursor::new(answers), &mut output, &path).is_err());
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains("already in use"), "{text}");
            assert!(text.contains(&addr.to_string()), "{text}");
            assert!(!path.exists());
        }
    }

    #[test]
    fn setup_retries_occupied_tcp_port_and_allows_same_port_for_udp() {
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let available = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let available_port = available.local_addr().unwrap().port();
        drop(available);
        for (protocol, listen_answers, expected_port) in [
            (
                "tcp",
                format!("127.0.0.1\n{occupied_port}\n127.0.0.1\n{available_port}"),
                available_port,
            ),
            ("udp", format!("127.0.0.1\n{occupied_port}"), occupied_port),
        ] {
            let path = test_path();
            let answers =
                format!("relay\n{protocol}\n{listen_answers}\n127.0.0.1\n25566\nn\n\n\ny\n");
            run_with(&mut Cursor::new(answers), &mut Vec::new(), &path).unwrap();
            let config = FileConfig::parse_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(config.redirects[0].listen.port(), expected_port);
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn tailscale_picker_selects_address_and_allows_manual_return() {
        let devices = vec![crate::tailscale::DeviceAddress {
            label: "backend — fd7a:115c:a1e0::2 (online)".into(),
            ip: "fd7a:115c:a1e0::2".parse().unwrap(),
        }];
        let mut output = Vec::new();
        assert_eq!(
            choose_tailscale(&mut Cursor::new("1\n"), &mut output, &devices).unwrap(),
            Some(devices[0].ip)
        );
        assert!(String::from_utf8(output.clone())
            .unwrap()
            .contains("backend"));
        assert_eq!(
            choose_tailscale(&mut Cursor::new("back\n"), &mut output, &devices).unwrap(),
            None
        );
        let current = "100.64.0.1:25565".parse().unwrap();
        assert_eq!(
            edit_socket(&mut Cursor::new("\n25566\n"), &mut output, current, true)
                .unwrap()
                .to_string(),
            "100.64.0.1:25566"
        );
        assert_eq!(
            edit_socket(&mut Cursor::new("back\n"), &mut output, current, false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
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
        let listener = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let answers = format!("bedrock-relay\nudp\n0.0.0.0\n{port}\n10.0.0.20\n19133\ny\n10.0.0.21\n19133\nn\n\n1\n\nOffline\nTry later\ny\n");
        let mut output = Vec::new();
        run_with(&mut Cursor::new(answers), &mut output, &path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        let config = FileConfig::parse_str(&saved).unwrap();
        let redirect = &config.redirects[0];
        assert_eq!(redirect.name.as_deref(), Some("bedrock-relay"));
        assert_eq!(redirect.listen.to_string(), format!("0.0.0.0:{port}"));
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let answers = format!("relay\ntcp\n127.0.0.1\n{port}\n127.0.0.1\n25566\nn\n\n\ny\n");
        run_with(&mut Cursor::new(answers), &mut Vec::new(), &path).unwrap();
        let answers =
            format!("relay\nrelay-two\ntcp\n127.0.0.1\n{port}\n127.0.0.1\n25568\nn\n\n\ny\n");
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
