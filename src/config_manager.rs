//! Named redirect changes that preserve the rest of the TOML file verbatim.

use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::{FileConfig, RedirectConfig, DEFAULT_CONFIG_TOML};
use crate::edit_config::{launch_editor, EditorOutcome};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub fn default_path() -> PathBuf {
    #[cfg(windows)]
    {
        let roaming = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(|home| PathBuf::from(home).join("AppData").join("Roaming"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        roaming.join("redir-rust").join("config.toml")
    }
    #[cfg(not(windows))]
    {
        let dir = Path::new("/etc/local/redir-rust");
        if dir.join("config.toml").exists()
            || dir.join("settings.json").exists()
            || dir_is_writable(dir)
        {
            dir.join("config.toml")
        } else {
            // Not root and no system config: use the current directory.
            PathBuf::from("config.toml")
        }
    }
}

/// True when `dir` can be created (if missing) and written to by this user.
#[cfg(not(windows))]
fn dir_is_writable(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".write-test-{}", std::process::id()));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub fn settings_path() -> PathBuf {
    default_path().with_file_name("settings.json")
}

pub fn configured_path(settings: &Path) -> io::Result<PathBuf> {
    let contents = match fs::read_to_string(settings) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(settings.with_file_name("config.toml"));
        }
        Err(err) => return Err(err),
    };
    #[derive(serde::Deserialize)]
    struct Settings {
        config_path: PathBuf,
    }
    let parsed: Settings = serde_json::from_str(&contents).map_err(io::Error::other)?;
    if parsed.config_path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "config_path is empty",
        ));
    }
    Ok(if parsed.config_path.is_absolute() {
        parsed.config_path
    } else {
        settings
            .parent()
            .unwrap_or(Path::new("."))
            .join(parsed.config_path)
    })
}

pub fn name_exists(path: &Path, name: &str) -> Result<bool> {
    let contents = read_or_default(path)?;
    let config: FileConfig = toml::from_str(&contents)?;
    Ok(config
        .redirects
        .iter()
        .any(|redirect| redirect.name.as_deref() == Some(name)))
}

/// Save a complete config from the graphical editor after validating it.
pub fn save_text(path: &Path, text: &str) -> Result<()> {
    let parsed: FileConfig = toml::from_str(text)?;
    if !parsed.redirects.is_empty() {
        FileConfig::parse_str(text)?;
    }
    write_atomic(path, text)?;
    Ok(())
}

pub fn add(path: &Path, redirect: RedirectConfig) -> Result<()> {
    let name = redirect
        .name
        .as_deref()
        .ok_or_else(|| invalid("--add requires --name"))?;
    if name.trim().is_empty() {
        return Err(invalid("redirect name cannot be empty").into());
    }
    let original = read_or_default(path)?;
    let existing: FileConfig = toml::from_str(&original)?;
    if existing
        .redirects
        .iter()
        .any(|item| item.name.as_deref() == Some(name))
    {
        return Err(invalid(format!("redirect name {name:?} already exists")).into());
    }
    let addition = toml::to_string(&FileConfig {
        redirects: vec![redirect],
    })?;
    let candidate = format!("{}\n{}", original.trim_end(), addition);
    FileConfig::parse_str(&candidate)?;
    write_atomic(path, &candidate)?;
    Ok(())
}

pub fn remove(path: &Path, name: &str) -> Result<bool> {
    let original = fs::read_to_string(path)?;
    let config = FileConfig::parse_str(&original)?;
    let index = find_name(&config, name)?;
    let ranges = redirect_ranges(&original);
    let (start, end) = *ranges
        .get(index)
        .ok_or_else(|| invalid("unsupported [[redirect]] header syntax"))?;
    let candidate = format!("{}{}", &original[..start], &original[end..]);
    let last_redirect = config.redirects.len() == 1;
    let candidate = if last_redirect {
        if candidate.trim().is_empty() {
            DEFAULT_CONFIG_TOML.to_string()
        } else {
            candidate
        }
    } else {
        FileConfig::parse_str(&candidate)?;
        candidate
    };
    write_atomic(path, &candidate)?;
    Ok(last_redirect)
}

pub fn edit(path: &Path, name: &str) -> Result<()> {
    let original = fs::read_to_string(path)?;
    let config = FileConfig::parse_str(&original)?;
    let index = find_name(&config, name)?;
    let ranges = redirect_ranges(&original);
    let (start, end) = *ranges
        .get(index)
        .ok_or_else(|| invalid("unsupported [[redirect]] header syntax"))?;
    let scratch = std::env::temp_dir().join(format!("redir-rust-edit-{}.toml", std::process::id()));
    fs::write(&scratch, &original[start..end])?;
    let edit_result = (|| -> Result<String> {
        match launch_editor(&scratch)? {
            EditorOutcome::Ran(status) if status.success() => {}
            EditorOutcome::Ran(status) => {
                return Err(io::Error::other(format!("editor exited with {status}")).into())
            }
            EditorOutcome::NoneFound(tried) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no editor found (tried: {})", tried.join(", ")),
                )
                .into())
            }
        }
        let edited = fs::read_to_string(&scratch)?;
        let one = FileConfig::parse_str(&edited)?;
        if one.redirects.len() != 1 {
            return Err(invalid("edited block must contain exactly one [[redirect]]").into());
        }
        if one.redirects[0].name.is_none() {
            return Err(invalid("edited redirect must keep a name").into());
        }
        let candidate = format!("{}{}{}", &original[..start], edited, &original[end..]);
        FileConfig::parse_str(&candidate)?;
        Ok(candidate)
    })();
    let _ = fs::remove_file(&scratch);
    write_atomic(path, &edit_result?)?;
    Ok(())
}

/// Replace one redirect after validation, leaving other blocks verbatim.
/// The index also permits editing older configs with unnamed redirects.
pub fn update(path: &Path, index: usize, redirect: RedirectConfig) -> Result<()> {
    let original = fs::read_to_string(path)?;
    let config = FileConfig::parse_str(&original)?;
    if index >= config.redirects.len() {
        return Err(invalid("selected service no longer exists").into());
    }
    let ranges = redirect_ranges(&original);
    let (start, end) = *ranges
        .get(index)
        .ok_or_else(|| invalid("unsupported [[redirect]] header syntax"))?;
    let edited = toml::to_string(&FileConfig {
        redirects: vec![redirect],
    })?;
    let candidate = format!("{}{}\n{}", &original[..start], edited, &original[end..]);
    FileConfig::parse_str(&candidate)?;
    write_atomic(path, &candidate)?;
    Ok(())
}

fn find_name(config: &FileConfig, name: &str) -> Result<usize> {
    config
        .redirects
        .iter()
        .position(|item| item.name.as_deref() == Some(name))
        .ok_or_else(|| invalid(format!("redirect name {name:?} not found")).into())
}

fn read_or_default(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(DEFAULT_CONFIG_TOML.to_string()),
        Err(err) => Err(err),
    }
}

fn redirect_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let header = line.split('#').next().unwrap_or("");
        if header
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            == "[[redirect]]"
        {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(i, start)| (*start, starts.get(i + 1).copied().unwrap_or(text.len())))
        .collect()
}

fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    fs::write(&tmp, text)?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(&tmp, metadata.permissions())?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
