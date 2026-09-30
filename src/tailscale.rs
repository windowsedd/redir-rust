use std::io::{self, Read};
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct DeviceAddress {
    pub label: String,
    pub ip: IpAddr,
}

/// Fetch on demand; a stopped daemon must not leave the setup prompt hanging.
pub fn fetch(local: bool) -> io::Result<Vec<DeviceAddress>> {
    fetch_with(&mut Command::new("tailscale"), local)
}

fn fetch_with(command: &mut Command, local: bool) -> io::Result<Vec<DeviceAddress>> {
    let mut child = command
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(4 * 1024 * 1024).read_to_end(&mut bytes)?;
        Ok::<_, io::Error>(bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Tailscale timed out",
                ))
            }
            Err(err) => break Err(err),
        }
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let bytes = reader
        .join()
        .map_err(|_| io::Error::other("Tailscale reader failed"))??;
    if !result?.success() {
        return Err(io::Error::other(
            "Tailscale status failed; check that Tailscale is running and signed in",
        ));
    }
    let status: serde_json::Value = serde_json::from_slice(&bytes)?;
    if status["BackendState"]
        .as_str()
        .is_some_and(|state| state != "Running")
    {
        return Err(io::Error::other("Tailscale is not running or signed in"));
    }
    Ok(addresses(&status, local))
}

fn addresses(status: &serde_json::Value, local: bool) -> Vec<DeviceAddress> {
    let devices: Vec<_> = if local {
        vec![&status["Self"]]
    } else {
        status["Peer"]
            .as_object()
            .map(|peers| peers.values().collect())
            .unwrap_or_default()
    };
    let mut result = Vec::new();
    for device in devices {
        let name = device["HostName"]
            .as_str()
            .filter(|name| !name.is_empty())
            .or_else(|| device["DNSName"].as_str())
            .unwrap_or("Unnamed device");
        let online = if device["Online"].as_bool() == Some(true) {
            "online"
        } else {
            "offline"
        };
        if let Some(ips) = device["TailscaleIPs"].as_array() {
            for ip in ips {
                if let Some(ip) = ip.as_str().and_then(|ip| ip.parse::<IpAddr>().ok()) {
                    result.push(DeviceAddress {
                        label: format!("{name} — {ip} ({online})"),
                        ip,
                    });
                }
            }
        }
    }
    result.sort_by(|a, b| a.label.cmp(&b.label));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_local_listen_addresses_from_peer_backends() {
        let status = serde_json::json!({
            "Self": {"HostName": "this-host", "Online": true,
                "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"]},
            "Peer": {
                "a": {"HostName": "server", "Online": true,
                    "TailscaleIPs": ["100.64.0.2", "invalid"]},
                "b": {"DNSName": "offline.tailnet.ts.net.", "Online": false,
                    "TailscaleIPs": ["100.64.0.3"]}
            }
        });
        let local = addresses(&status, true);
        assert_eq!(local.len(), 2);
        assert_eq!(local[0].ip.to_string(), "100.64.0.1");
        assert!(local[0].label.contains("this-host"));
        let peers = addresses(&status, false);
        assert_eq!(peers.len(), 2);
        assert!(peers
            .iter()
            .any(|p| p.label.contains("server") && p.label.contains("online")));
        assert!(peers
            .iter()
            .any(|p| p.label.contains("offline.tailnet.ts.net") && p.label.contains("offline")));
        assert!(addresses(&serde_json::json!({"Self": null, "Peer": null}), false).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn fetch_reads_command_output_and_reports_unavailable_status() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", r#"printf '%s' '{"BackendState":"Running","Self":{"HostName":"local","Online":true,"TailscaleIPs":["100.64.0.1"]}}'"#]);
        let devices = fetch_with(&mut command, true).unwrap();
        assert_eq!(devices[0].ip.to_string(), "100.64.0.1");
        for script in [
            "exit 1",
            "printf invalid",
            "printf '%s' '{\"BackendState\":\"NeedsLogin\"}'",
        ] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script]);
            assert!(fetch_with(&mut command, true).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn fetch_times_out_and_reaps_unresponsive_command() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec /bin/sleep 30"]);
        let started = Instant::now();
        assert_eq!(
            fetch_with(&mut command, true).err().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
