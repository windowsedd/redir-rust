//! Background process control for Windows, where this project has no service unit.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::FileConfig;
use crate::config_manager::{configured_path, default_path, settings_path};

const DETACHED_PROCESS: u32 = 0x0000_0008;

fn pid_path() -> PathBuf {
    default_path().with_file_name("redir-rust.pid")
}

fn read_pid() -> io::Result<Option<u32>> {
    match fs::read_to_string(pid_path()) {
        Ok(value) => value
            .trim()
            .parse::<u32>()
            .map(Some)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn is_running(pid: u32) -> io::Result<bool> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("tasklist failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let mut fields = line.split(',');
        fields.next().is_some_and(|name| {
            name.trim_matches('"')
                .eq_ignore_ascii_case("redir-rust.exe")
        }) && fields
            .next()
            .is_some_and(|value| value.trim_matches('"').parse::<u32>().ok() == Some(pid))
    }))
}

pub fn start() -> io::Result<()> {
    if let Some(pid) = read_pid()? {
        if is_running(pid)? {
            println!("redir-rust is already running (PID {pid})");
            return Ok(());
        }
    }
    let config = configured_path(&settings_path())?;
    FileConfig::load(&config).map_err(io::Error::other)?;
    let pid_file = pid_path();
    fs::create_dir_all(pid_file.parent().unwrap())?;
    let log_path = config.with_file_name("redir-rust.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let stderr = log.try_clone()?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .creation_flags(DETACHED_PROCESS)
        .spawn()?;
    std::thread::sleep(Duration::from_millis(150));
    if let Some(exit) = child.try_wait()? {
        return Err(io::Error::other(format!(
            "redir-rust exited during startup ({exit}); see {}",
            log_path.display()
        )));
    }
    fs::write(pid_file, child.id().to_string())?;
    println!(
        "redir-rust started (PID {}, log: {})",
        child.id(),
        log_path.display()
    );
    Ok(())
}

pub fn stop() -> io::Result<()> {
    let Some(pid) = read_pid()? else {
        println!("redir-rust is stopped");
        return Ok(());
    };
    if !is_running(pid)? {
        fs::remove_file(pid_path())?;
        println!("redir-rust is stopped");
        return Ok(());
    }
    let exit = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()?;
    if !exit.success() {
        return Err(io::Error::other(format!("taskkill failed ({exit})")));
    }
    fs::remove_file(pid_path())?;
    println!("redir-rust stopped");
    Ok(())
}

pub fn running() -> io::Result<bool> {
    match read_pid()? {
        Some(pid) => is_running(pid),
        None => Ok(false),
    }
}

pub fn status() -> io::Result<bool> {
    let running = match read_pid()? {
        Some(pid) if is_running(pid)? => {
            println!("redir-rust is running (PID {pid})");
            true
        }
        _ => {
            println!("redir-rust is stopped");
            false
        }
    };
    Ok(running)
}
