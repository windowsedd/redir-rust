//! Browser manager with a per-run access token for network access.

use std::error::Error;
use std::fs;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

use redir_rust::config::{
    BedrockConfig, BedrockPluginsConfig, FileConfig, MinecraftConfig, MinecraftPluginsConfig,
    Protocol, RedirectConfig, TargetConfig, DEFAULT_CONFIG_TOML,
};
use redir_rust::config_manager;
use redir_rust::shaping::WaitInOut;
use serde_json::{json, Value};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const PAGE: &str = include_str!("../ui/index.html");

pub fn run(path: &Path, bind: SocketAddr) -> ExitCode {
    match serve(path, bind) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: GUI failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn serve(path: &Path, bind: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(bind)?;
    let address = listener.local_addr()?;
    let token = token();
    let access_address = suggested_address(address);
    let url = format!("http://{access_address}/?token={token}");
    println!("redir-rust GUI listening on {address}");
    println!("Open: {url}");
    if address.ip().is_unspecified() {
        println!("If this is not the right network IP, use another address of this machine with the same port and token.");
    }
    if std::env::var_os("REDIR_RUST_GUI_NO_OPEN").is_none() {
        open_browser(&url);
    }
    println!("Press Ctrl+C to close the GUI.");
    for connection in listener.incoming() {
        let stream = connection?;
        if let Err(err) = handle(stream, path, &token, address) {
            eprintln!("GUI request: {err}");
        }
    }
    Ok(())
}

fn suggested_address(bound: SocketAddr) -> SocketAddr {
    if !bound.ip().is_unspecified() {
        return bound;
    }
    let detected = UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("192.0.2.1:9")?;
            socket.local_addr()
        })
        .ok()
        .filter(|address| !address.ip().is_loopback() && !address.ip().is_unspecified());
    SocketAddr::new(
        detected
            .map(|address| address.ip())
            .unwrap_or_else(|| "127.0.0.1".parse().unwrap()),
        bound.port(),
    )
}

fn token() -> String {
    let half = || {
        let random = std::collections::hash_map::RandomState::new();
        let mut hasher = random.build_hasher();
        std::process::id().hash(&mut hasher);
        std::time::SystemTime::now().hash(&mut hasher);
        hasher.finish()
    };
    format!("{:016x}{:016x}", half(), half())
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let result = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(windows))]
    let result = Command::new("xdg-open").arg(url).spawn();
    if let Err(err) = result {
        eprintln!("Could not open a browser ({err}); open the URL above manually.");
    }
}

fn handle(mut stream: TcpStream, path: &Path, token: &str, address: SocketAddr) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let route = parts.next().unwrap_or("");
    let mut length = 0;
    let mut valid_host = false;
    let mut valid_token = false;
    let mut valid_type = false;
    for _ in 0..64 {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                valid_host = value.parse::<SocketAddr>().is_ok_and(|host| {
                    host.port() == address.port()
                        && !host.ip().is_unspecified()
                        && (address.ip().is_unspecified() || host.ip() == address.ip())
                });
            }
            "content-length" => length = value.parse::<usize>().unwrap_or(usize::MAX),
            "x-redir-token" => valid_token = value == token,
            "content-type" => valid_type = value.starts_with("application/json"),
            _ => {}
        }
    }
    if !valid_host || length > 1_048_576 {
        return respond(&mut stream, 400, "text/plain", "Invalid request");
    }
    if method == "GET" && route == format!("/?token={token}") {
        return respond(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            &PAGE.replace("__TOKEN__", token),
        );
    }
    if method == "GET" && route == "/api/state" && valid_token {
        return json_response(&mut stream, 200, state(path));
    }
    if method != "POST" || !valid_token || !valid_type {
        return respond(&mut stream, 403, "text/plain", "Forbidden");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let result = serde_json::from_slice::<Value>(&body)
        .map_err(|err| err.into())
        .and_then(|input| action(route, path, &input));
    match result {
        Ok(message) => json_response(&mut stream, 200, json!({"ok": true, "message": message})),
        Err(err) => json_response(
            &mut stream,
            400,
            json!({"ok": false, "message": err.to_string()}),
        ),
    }
}

fn respond(stream: &mut TcpStream, code: u16, kind: &str, body: &str) -> Result<()> {
    write!(stream, "HTTP/1.1 {code} {}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'\r\n\r\n{body}", if code == 200 { "OK" } else { "Error" }, body.len())?;
    Ok(())
}

fn json_response(stream: &mut TcpStream, code: u16, value: Value) -> Result<()> {
    respond(
        stream,
        code,
        "application/json; charset=utf-8",
        &value.to_string(),
    )
}

fn state(path: &Path) -> Value {
    let (text, read_error) = match fs::read_to_string(path) {
        Ok(text) => (text, Value::Null),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            (DEFAULT_CONFIG_TOML.to_string(), Value::Null)
        }
        Err(err) => (String::new(), json!(err.to_string())),
    };
    let parsed: std::result::Result<FileConfig, _> = toml::from_str(&text);
    let (redirects, error) = match parsed {
        Ok(config) => (json!(config.redirects), Value::Null),
        Err(err) => (json!([]), json!(err.to_string())),
    };
    json!({"path": path.display().to_string(), "text": text, "redirects": redirects,
        "error": if read_error.is_null() { error } else { read_error }, "running": running().ok()})
}

fn field<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Missing {key}").into())
}

fn action(route: &str, path: &Path, input: &Value) -> Result<String> {
    match route {
        "/api/add" => {
            let name = field(input, "name")?.trim();
            if name.is_empty() {
                return Err("Service name is required".into());
            }
            let listen: SocketAddr = field(input, "listen")?.parse()?;
            let protocol = match field(input, "protocol")? {
                "tcp" => Protocol::Tcp,
                "udp" => Protocol::Udp,
                _ => return Err("Choose TCP or UDP".into()),
            };
            let targets = input["targets"]
                .as_array()
                .ok_or("Add at least one backend target")?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .ok_or("Invalid target")?
                        .parse::<SocketAddr>()
                        .map_err(Into::into)
                })
                .collect::<Result<Vec<_>>>()?;
            if targets.is_empty() {
                return Err("Add at least one backend target".into());
            }
            let plugin = input["plugin"].as_bool().unwrap_or(false);
            let text = |key: &str| {
                input[key]
                    .as_str()
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_owned)
            };
            let minecraft = if plugin && protocol == Protocol::Tcp {
                Some(MinecraftConfig {
                    plugins: Some(MinecraftPluginsConfig {
                        enabled: true,
                        status_line: text("status_line"),
                        motd_line1: text("motd_line1"),
                        motd_line2: text("motd_line2"),
                        favicon_path: text("favicon_path").map(Into::into),
                    }),
                })
            } else {
                None
            };
            let bedrock = if plugin && protocol == Protocol::Udp {
                Some(BedrockConfig {
                    plugins: Some(BedrockPluginsConfig {
                        enabled: true,
                        motd_line1: text("motd_line1"),
                        motd_line2: text("motd_line2"),
                    }),
                })
            } else {
                None
            };
            if let Some(favicon) = minecraft
                .as_ref()
                .and_then(|value| value.plugins.as_ref())
                .and_then(|value| value.favicon_path.as_ref())
            {
                if !favicon.is_absolute() || !favicon.is_file() {
                    return Err("Favicon must be an existing absolute file path".into());
                }
            }
            let timeout = input["timeout_ms"]
                .as_u64()
                .ok_or("Timeout must be a positive number")?;
            if timeout == 0 {
                return Err("Timeout must be positive".into());
            }
            config_manager::add(
                path,
                RedirectConfig {
                    name: Some(name.into()),
                    listen,
                    target_config: TargetConfig::from_ordered(targets),
                    protocol,
                    connect_timeout_ms: if protocol == Protocol::Tcp {
                        timeout
                    } else {
                        3000
                    },
                    udp_idle_timeout_ms: if protocol == Protocol::Udp {
                        timeout
                    } else {
                        60000
                    },
                    minecraft,
                    bedrock,
                    max_bandwidth_bps: None,
                    wait_in_out: WaitInOut::Both,
                    random_wait_ms: None,
                    bufsize_bytes: 16 * 1024,
                },
            )?;
            Ok(format!("Saved {name}. Reload to apply changes."))
        }
        "/api/remove" => {
            let name = field(input, "name")?;
            config_manager::remove(path, name)?;
            Ok(format!("Removed {name}. Reload to apply changes."))
        }
        "/api/save" => {
            config_manager::save_text(path, field(input, "text")?)?;
            Ok("Config saved. Reload to apply changes.".into())
        }
        "/api/service" => {
            let choice = field(input, "action")?;
            if !["start", "stop", "restart", "reload"].contains(&choice) {
                return Err("Invalid service action".into());
            }
            let mut command = Command::new(std::env::current_exe()?);
            command.arg(format!("--{choice}"));
            if choice == "reload" {
                command.arg("--config").arg(path);
            }
            let output = command.output()?;
            let message = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if !output.status.success() {
                return Err(message.into());
            }
            Ok(if message.trim().is_empty() {
                format!("Service {choice} complete")
            } else {
                message.trim().into()
            })
        }
        _ => Err("Unknown action".into()),
    }
}

#[cfg(not(windows))]
fn running() -> io::Result<bool> {
    Ok(Command::new("systemctl")
        .args(["is-active", "--quiet", "redir-rust.service"])
        .status()?
        .success())
}

#[cfg(windows)]
fn running() -> io::Result<bool> {
    redir_rust::windows_process::status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn network_page_requires_access_token() {
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let bound = listener.local_addr().unwrap();
        assert_eq!(bound.ip(), IpAddr::from([0, 0, 0, 0]));
        for (route, expected) in [
            ("/", "403"),
            ("/api/state", "403"),
            ("/?token=secret", "200"),
        ] {
            let request = format!(
                "GET {route} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                bound.port()
            );
            let worker = std::thread::spawn({
                let listener = listener.try_clone().unwrap();
                move || {
                    let (stream, _) = listener.accept().unwrap();
                    handle(stream, Path::new("/tmp/unused.toml"), "secret", bound).unwrap();
                }
            });
            let mut client = TcpStream::connect(("127.0.0.1", bound.port())).unwrap();
            client.write_all(request.as_bytes()).unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            assert!(
                response.starts_with(&format!("HTTP/1.1 {expected}")),
                "{response}"
            );
            worker.join().unwrap();
        }
    }

    #[test]
    fn graphical_add_keeps_order_and_rejects_duplicate_names() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("redir-gui-{}-{unique}.toml", std::process::id()));
        let input = json!({"name":"bedrock-relay","protocol":"udp","listen":"0.0.0.0:19132",
            "targets":["10.0.0.20:19133","10.0.0.21:19133"],"timeout_ms":60000,
            "plugin":true,"motd_line1":"Offline"});
        action("/api/add", &path, &input).unwrap();
        assert!(action("/api/add", &path, &input).is_err());
        let text = fs::read_to_string(&path).unwrap();
        let parsed = FileConfig::parse_str(&text).unwrap();
        let redirect = &parsed.redirects[0];
        assert_eq!(
            redirect
                .target_config
                .as_slice()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["10.0.0.20:19133", "10.0.0.21:19133"]
        );
        assert!(
            redirect
                .bedrock
                .as_ref()
                .unwrap()
                .plugins
                .as_ref()
                .unwrap()
                .enabled
        );
        let bad = text.replacen("name = \"bedrock-relay\"", "name = \"\"", 1);
        assert!(action("/api/save", &path, &json!({"text":bad})).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        fs::remove_file(path).unwrap();
    }
}
