use std::process::Command;

fn temp_config() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "redir-rust-management-{}-{}.toml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn settings_selects_config_for_add() {
    let directory = temp_config().with_extension("");
    std::fs::create_dir_all(&directory).unwrap();
    let settings = directory.join("settings.json");
    std::fs::write(&settings, r#"{"config_path":"custom/config.toml"}"#).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_redir-rust"))
        .args([
            "--add",
            "--settings",
            settings.to_str().unwrap(),
            "--name",
            "main",
            "--listen",
            "127.0.0.1:25565",
            "--target",
            "127.0.0.1:25566",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let config =
        redir_rust::config::FileConfig::load(directory.join("custom/config.toml")).unwrap();
    assert_eq!(config.redirects[0].name.as_deref(), Some("main"));
    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn add_rejects_duplicate_name_and_remove_preserves_other_redirects() {
    let path = temp_config();
    let binary = env!("CARGO_BIN_EXE_redir-rust");
    let add = |name: &str, port: &str| {
        Command::new(binary)
            .args([
                "--add",
                "--config",
                path.to_str().unwrap(),
                "--name",
                name,
                "--listen",
                port,
                "--target",
                "127.0.0.1:25566",
            ])
            .status()
            .unwrap()
    };
    assert!(add("first", "127.0.0.1:25565").success());
    let before = std::fs::read_to_string(&path).unwrap();
    assert!(!add("first", "127.0.0.1:25567").success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert!(add("second", "127.0.0.1:25567").success());

    let remove = |name: &str| {
        Command::new(binary)
            .args(["--remove", name, "--config", path.to_str().unwrap()])
            .status()
            .unwrap()
    };
    assert!(remove("first").success());
    let config = redir_rust::config::FileConfig::load(&path).unwrap();
    assert_eq!(config.redirects.len(), 1);
    assert_eq!(config.redirects[0].name.as_deref(), Some("second"));
    assert!(remove("second").success());
    let inactive: redir_rust::config::FileConfig =
        toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(inactive.redirects.is_empty());
    std::fs::remove_file(path).ok();
}

#[cfg(unix)]
#[test]
fn edit_rejects_duplicate_name_without_changing_config() {
    use std::os::unix::fs::PermissionsExt;

    let path = temp_config();
    let binary = env!("CARGO_BIN_EXE_redir-rust");
    for (name, port) in [("first", "25565"), ("second", "25567")] {
        assert!(Command::new(binary)
            .args([
                "--add",
                "--config",
                path.to_str().unwrap(),
                "--name",
                name,
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--target",
                "127.0.0.1:25566"
            ])
            .status()
            .unwrap()
            .success());
    }
    let before = std::fs::read_to_string(&path).unwrap();
    let editor = path.with_extension("sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\nsed -i 's/name = \"first\"/name = \"second\"/' \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let status = Command::new(binary)
        .args(["--edit", "first", "--config", path.to_str().unwrap()])
        .env("EDITOR", &editor)
        .status()
        .unwrap();
    assert!(!status.success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

    std::fs::write(
        &editor,
        "#!/bin/sh\nsed -i 's/name = \"first\"/name = \"third\"/' \"$1\"\n",
    )
    .unwrap();
    let status = Command::new(binary)
        .args(["--edit", "first", "--config", path.to_str().unwrap()])
        .env("EDITOR", &editor)
        .status()
        .unwrap();
    assert!(status.success());
    let names: Vec<_> = redir_rust::config::FileConfig::load(&path)
        .unwrap()
        .redirects
        .into_iter()
        .map(|r| r.name.unwrap())
        .collect();
    assert_eq!(names, ["third", "second"]);
    std::fs::remove_file(path).ok();
    std::fs::remove_file(editor).ok();
}

#[test]
fn add_roundtrips_plugin_settings() {
    let path = temp_config();
    let status = Command::new(env!("CARGO_BIN_EXE_redir-rust"))
        .args([
            "--add",
            "--config",
            path.to_str().unwrap(),
            "--name",
            "offline",
            "--listen",
            "127.0.0.1:25565",
            "--target",
            "127.0.0.1:25566",
            "--minecraft-offline-motd",
            "--status-line",
            "custom",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let config = redir_rust::config::FileConfig::load(&path).unwrap();
    assert_eq!(
        config.redirects[0]
            .minecraft_plugins()
            .unwrap()
            .status_line
            .as_deref(),
        Some("custom")
    );
    std::fs::remove_file(path).ok();
}

#[test]
fn add_accepts_relative_config_filename() {
    let directory = temp_config().with_extension("");
    std::fs::create_dir_all(&directory).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_redir-rust"))
        .current_dir(&directory)
        .args([
            "--add",
            "--config",
            "config.toml",
            "--name",
            "local",
            "--listen",
            "127.0.0.1:25565",
            "--target",
            "127.0.0.1:25566",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        redir_rust::config::FileConfig::load(directory.join("config.toml"))
            .unwrap()
            .redirects
            .len(),
        1
    );
    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn run_mode_rejects_redirect_flags_ignored_by_config() {
    let output = Command::new(env!("CARGO_BIN_EXE_redir-rust"))
        .args([
            "--config",
            "missing-config.toml",
            "--listen",
            "127.0.0.1:25565",
            "--target",
            "127.0.0.1:25566",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("redirect flags cannot be combined"));
}

#[test]
fn remove_accepts_header_with_inline_comment() {
    let path = temp_config();
    std::fs::write(&path, "# keep this note\n[[redirect]] # first\nname = 'first'\nlisten = '127.0.0.1:25565'\ntarget = '127.0.0.1:25566'\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_redir-rust"))
        .args(["--remove", "first", "--config", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("# keep this note"));
    std::fs::remove_file(path).ok();
}
