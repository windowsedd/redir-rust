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
