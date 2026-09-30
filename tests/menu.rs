use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn bare_invocation_offers_menu_and_exit() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_redir-rust"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"8\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let menu = String::from_utf8(output.stdout).unwrap();
    assert!(menu.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    assert!(menu.contains("windowsedd"));
    for item in [
        "Start",
        "Stop",
        "Setup Config",
        "Edit Config",
        "Status",
        "Monitor",
        "Open GUI",
        "Exit",
    ] {
        assert!(menu.contains(item), "missing {item} from menu: {menu}");
    }
}

#[cfg(unix)]
#[test]
fn menu_shows_service_state_without_systemctl_diagnostics() {
    let dir = std::env::temp_dir().join(format!("redir-menu-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let systemctl = dir.join("systemctl");
    std::fs::write(
        &systemctl,
        "#!/bin/sh\necho inactive\necho diagnostics >&2\nexit 3\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_redir-rust"))
        .env("PATH", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"8\n").unwrap();
    let output = child.wait_with_output().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    let menu = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());
    assert!(menu.contains("Stopped"), "{menu}");
    assert!(menu.contains("0 redirects"), "{menu}");
    assert!(menu.contains("0 connections"), "{menu}");
    assert!(output.stderr.is_empty());
}
