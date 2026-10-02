//! Acknowledged reload requests over an authenticated loopback socket.
use std::fs::{self, File};
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream as AsyncTcpStream};

#[derive(Deserialize, Serialize)]
struct Endpoint {
    address: SocketAddr,
    token: String,
    pid: u32,
}

fn endpoint_path(config: &Path) -> PathBuf {
    let mut path = config.as_os_str().to_os_string();
    path.push(".reload.json");
    path.into()
}

fn read_endpoint(path: &Path) -> io::Result<Endpoint> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(io::Error::other("reload endpoint file is too large"));
    }
    let endpoint: Endpoint = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if endpoint.address.ip() != std::net::Ipv4Addr::LOCALHOST
        || endpoint.token.len() != 32
        || !endpoint.token.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(io::Error::other("invalid reload endpoint"));
    }
    Ok(endpoint)
}

fn send(endpoint: &Endpoint, command: &str) -> io::Result<()> {
    let mut stream = TcpStream::connect_timeout(&endpoint.address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "{} {command}", endpoint.token)?;
    stream.shutdown(Shutdown::Write)?;
    let mut response = String::new();
    stream.take(16_385).read_to_string(&mut response)?;
    if response.trim() == "OK" {
        Ok(())
    } else {
        Err(io::Error::other(
            response
                .trim()
                .strip_prefix("ERROR ")
                .unwrap_or("reload server returned an invalid response"),
        ))
    }
}

/// Reloads the process using this config file, returning only after success
/// or rejection. Reading the owner-only endpoint file authorizes the request.
pub fn request(config: &Path) -> io::Result<()> {
    let config = fs::canonicalize(config)?;
    let endpoint = read_endpoint(&endpoint_path(&config)).map_err(|err| {
        io::Error::new(err.kind(), format!("no accessible reload endpoint for {}: {err}; is a reload-capable instance running?", config.display()))
    })?;
    send(&endpoint, "reload")
}

pub(crate) struct Server {
    pub listener: TcpListener,
    path: PathBuf,
    token: String,
}

impl Server {
    pub async fn bind(config: &Path) -> io::Result<Self> {
        let path = endpoint_path(config);
        if let Ok(existing) = read_endpoint(&path) {
            if tokio::task::spawn_blocking(move || send(&existing, "ping"))
                .await
                .map_err(io::Error::other)?
                .is_ok()
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another instance owns this config's reload endpoint",
                ));
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        // Same per-run random token pattern used by the browser manager.
        let half = || {
            let random = std::collections::hash_map::RandomState::new();
            let mut hasher = random.build_hasher();
            std::process::id().hash(&mut hasher);
            std::time::SystemTime::now().hash(&mut hasher);
            hasher.finish()
        };
        let token = format!("{:016x}{:016x}", half(), half());
        let endpoint = Endpoint {
            address: listener.local_addr()?,
            token: token.clone(),
            pid: std::process::id(),
        };
        let temp = path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos()
        ));
        let written = (|| -> io::Result<()> {
            let mut file = private_file(&temp)?;
            file.write_all(&serde_json::to_vec(&endpoint).map_err(io::Error::other)?)?;
            drop(file);
            // Windows rename cannot replace an existing stale endpoint.
            #[cfg(windows)]
            if path.exists() {
                fs::remove_file(&path)?;
            }
            fs::rename(&temp, &path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temp);
        }
        written?;
        Ok(Self {
            listener,
            path,
            token,
        })
    }

    pub fn authenticate(
        &self,
        mut stream: AsyncTcpStream,
    ) -> impl std::future::Future<Output = (AsyncTcpStream, io::Result<String>)> + Send + 'static
    {
        let token = self.token.clone();
        async move {
            let result = authenticate(&mut stream, &token).await;
            (stream, result)
        }
    }
}

async fn authenticate(stream: &mut AsyncTcpStream, token: &str) -> io::Result<String> {
    let mut bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        stream.take(513).read_to_end(&mut bytes),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "reload request timed out"))??;
    if bytes.len() > 512 {
        return Err(io::Error::other("reload request too large"));
    }
    let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    let fields: Vec<_> = text.split_whitespace().collect();
    if fields.len() != 2 || fields[0] != token || !["ping", "reload"].contains(&fields[1]) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid reload request or token",
        ));
    }
    Ok(fields[1].into())
}

impl Drop for Server {
    fn drop(&mut self) {
        if read_endpoint(&self.path).is_ok_and(|endpoint| endpoint.token == self.token) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) async fn respond(stream: &mut AsyncTcpStream, result: io::Result<()>) {
    let message = match result {
        Ok(()) => "OK\n".into(),
        Err(err) => format!("ERROR {}\n", err.to_string().replace(['\r', '\n'], " ")),
    };
    let _ =
        tokio::time::timeout(Duration::from_secs(2), stream.write_all(message.as_bytes())).await;
}

#[cfg(not(windows))]
fn private_file(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(windows)]
fn private_file(path: &Path) -> io::Result<File> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::ptr;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }
    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            path: *const u16,
            access: u32,
            share: u32,
            security: *const SecurityAttributes,
            disposition: u32,
            attributes: u32,
            template: *mut c_void,
        ) -> *mut c_void;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    // Protected DACL: only the file owner and SYSTEM receive access, even
    // when the config directory grants inherited access to other users.
    let sddl: Vec<u16> = "D:P(A;;FA;;;OW)(A;;FA;;;SY)\0".encode_utf16().collect();
    let mut descriptor = ptr::null_mut();
    // SAFETY: all pointers refer to live buffers; the descriptor is released
    // after CreateFileW, and ownership of a successful file handle moves to File.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let security = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            descriptor,
            inherit: 0,
        };
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // GENERIC_WRITE, no sharing, CREATE_NEW, FILE_ATTRIBUTE_NORMAL.
        let handle = CreateFileW(
            path.as_ptr(),
            0x40000000,
            0,
            &security,
            1,
            0x80,
            ptr::null_mut(),
        );
        let error = io::Error::last_os_error();
        LocalFree(descriptor);
        if handle as isize == -1 {
            Err(error)
        } else {
            Ok(File::from_raw_handle(handle))
        }
    }
}
