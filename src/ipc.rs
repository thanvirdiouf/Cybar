// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const MAX_LINE: usize = 1024 * 1024;
pub const DEFAULT_TIMEOUT_MS: u64 = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
impl Envelope {
    pub fn request(method: &str, params: Option<Value>, message: bool) -> Self {
        Self {
            id: if message { None } else { Some(Value::from(1)) },
            kind: if message { "msg" } else { "request" }.into(),
            method: Some(method.into()),
            params,
            result: None,
            error: None,
        }
    }
    pub fn reply(&self, result: Result<Value>) -> Self {
        let (value, error) = match result {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        Self {
            id: self.id.clone(),
            kind: "reply".into(),
            method: None,
            params: None,
            result: value,
            error,
        }
    }
    fn validate(&self) -> Result<()> {
        if self.kind != "request" && self.kind != "msg" {
            bail!("expected msg or request");
        }
        if self.method.as_deref().is_none_or(str::is_empty) {
            bail!("missing method");
        }
        if self.kind == "request"
            && !self
                .id
                .as_ref()
                .is_some_and(|v| v.is_number() || v.is_string())
        {
            bail!("request requires a numeric or string id");
        }
        Ok(())
    }
}

pub fn socket_path() -> Result<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let runtime = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(p) => {
            let p = PathBuf::from(p);
            let meta = fs::symlink_metadata(&p).context("XDG_RUNTIME_DIR does not exist")?;
            if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
                bail!(
                    "XDG_RUNTIME_DIR must be a directory owned by you and not writable by others"
                );
            }
            p
        }
        None => PathBuf::from(format!("/tmp/cybar-{uid}")),
    };
    let dir = if std::env::var_os("XDG_RUNTIME_DIR").is_some() {
        runtime.join("cybar")
    } else {
        runtime
    };
    match fs::create_dir(&dir) {
        Ok(()) => fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        bail!("{} must be owned by you with mode 0700", dir.display());
    }
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    // Encode every byte to avoid collisions between displays and prevent path traversal.
    let key: String = display.bytes().map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("display-{key}.sock"));
    if path.as_os_str().len() >= 104 {
        bail!("IPC socket path is too long; use a shorter XDG_RUNTIME_DIR or DISPLAY");
    }
    Ok(path)
}

struct Client {
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    used: Instant,
    eof: bool,
    pending: usize,
}
pub struct Pending {
    pub client: u64,
    pub envelope: Envelope,
}
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    _lock: File,
    clients: HashMap<u64, Client>,
    next_id: u64,
}
impl Server {
    pub fn bind(path: &Path) -> Result<Self> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.with_extension("lock"))?;
        if lock.metadata()?.uid() != unsafe { libc::geteuid() } {
            bail!("IPC lock belongs to another user");
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("Cybar is already running on this display");
        }
        if let Ok(meta) = fs::symlink_metadata(path) {
            if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
                bail!("refusing to replace unsafe IPC path {}", path.display());
            }
            if UnixStream::connect(path).is_ok() {
                bail!("Cybar is already running on this display");
            }
            fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path: path.to_owned(),
            _lock: lock,
            clients: HashMap::new(),
            next_id: 1,
        })
    }
    pub fn fd(&self) -> i32 {
        self.listener.as_raw_fd()
    }
    pub fn fds(&self) -> Vec<(i32, bool)> {
        self.clients
            .values()
            .map(|c| (c.stream.as_raw_fd(), !c.output.is_empty()))
            .collect()
    }
    pub fn pump(&mut self) -> Vec<Pending> {
        while let Ok((stream, _)) = self.listener.accept() {
            if self.clients.len() >= 64 {
                continue;
            }
            if stream.set_nonblocking(true).is_err() {
                continue;
            }
            self.clients.insert(
                self.next_id,
                Client {
                    stream,
                    input: vec![],
                    output: vec![],
                    used: Instant::now(),
                    eof: false,
                    pending: 0,
                },
            );
            self.next_id += 1;
        }
        let mut pending = vec![];
        self.clients.retain(|id, c| {
            let mut buf = [0u8; 8192];
            loop {
                match c.stream.read(&mut buf) {
                    Ok(0) => {
                        c.eof = true;
                        break;
                    }
                    Ok(n) => {
                        c.input.extend_from_slice(&buf[..n]);
                        c.used = Instant::now();
                        if c.input.len() > MAX_LINE {
                            return false;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return false,
                }
            }
            let mut messages = 0;
            while let Some(end) = c.input.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = c.input.drain(..=end).collect();
                messages += 1;
                if messages > 128 {
                    return false;
                }
                match serde_json::from_slice::<Envelope>(&line) {
                    Ok(envelope) => match envelope.validate() {
                        Ok(()) => {
                            if envelope.kind == "request" {
                                c.pending += 1;
                            }
                            pending.push(Pending {
                                client: *id,
                                envelope,
                            });
                        }
                        Err(e) => {
                            if envelope.kind == "request" {
                                if let Ok(mut b) = serde_json::to_vec(&envelope.reply(Err(e))) {
                                    b.push(b'\n');
                                    c.output.extend(b);
                                }
                            }
                        }
                    },
                    Err(e) => eprintln!("cybar: invalid IPC JSON: {e}"),
                }
            }
            c.used.elapsed() < Duration::from_secs(30)
        });
        self.flush();
        pending
    }
    pub fn reply(&mut self, client: u64, envelope: &Envelope, result: Result<Value>) {
        if envelope.kind != "request" {
            if let Err(e) = result {
                eprintln!("cybar: IPC: {e:#}");
            }
            return;
        }
        if let Some(c) = self.clients.get_mut(&client) {
            c.pending = c.pending.saturating_sub(1);
            match serde_json::to_vec(&envelope.reply(result)) {
                Ok(mut data) if data.len() <= MAX_LINE => {
                    data.push(b'\n');
                    c.output.extend(data);
                }
                _ => {
                    let mut data = serde_json::to_vec(
                        &envelope.reply(Err(anyhow::anyhow!("reply exceeds 1 MiB"))),
                    )
                    .unwrap();
                    data.push(b'\n');
                    c.output.extend(data);
                }
            }
        }
    }
    pub fn flush(&mut self) {
        self.clients.retain(|_, c| {
            while !c.output.is_empty() {
                match c.stream.write(&c.output) {
                    Ok(0) => return false,
                    Ok(n) => {
                        c.output.drain(..n);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return false,
                }
            }
            !(c.eof && c.output.is_empty() && c.pending == 0)
        });
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn send(path: &Path, envelope: Envelope, timeout_ms: u64) -> Result<Option<Value>> {
    let mut stream = UnixStream::connect(path)
        .with_context(|| format!("no Cybar instance is running ({})", path.display()))?;
    let timeout = Duration::from_millis(timeout_ms.max(1));
    stream.set_write_timeout(Some(timeout))?;
    let mut bytes = serde_json::to_vec(&envelope)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_LINE {
        bail!("message exceeds 1 MiB");
    }
    stream.write_all(&bytes)?;
    if envelope.kind == "msg" {
        return Ok(None);
    }
    let deadline = Instant::now() + timeout;
    let mut input = vec![];
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .context("request timed out")?;
        stream.set_read_timeout(Some(left))?;
        let mut buf = [0u8; 4096];
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                bail!("request timed out after {timeout_ms} ms")
            }
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            bail!("daemon closed the connection without replying");
        }
        input.extend_from_slice(&buf[..n]);
        if input.len() > MAX_LINE {
            bail!("reply exceeds 1 MiB");
        }
        while let Some(end) = input.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = input.drain(..=end).collect();
            let reply: Envelope = serde_json::from_slice(&line)?;
            if reply.kind != "reply" || reply.id != envelope.id {
                continue;
            }
            if let Some(error) = reply.error {
                bail!("{error}");
            }
            return Ok(Some(reply.result.unwrap_or(Value::Null)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn envelopes_are_symmetric() {
        let q = Envelope::request("reload", None, false);
        let r = q.reply(Ok(Value::Bool(true)));
        assert_eq!(r.id, q.id);
        assert_eq!(r.kind, "reply");
        assert!(r.error.is_none());
        assert!(
            serde_json::to_string(&q)
                .unwrap()
                .contains("\"type\":\"request\"")
        );
    }
    #[test]
    fn missing_request_id_is_rejected() {
        let mut e = Envelope::request("quit", None, false);
        e.id = None;
        assert!(e.validate().is_err());
        e.kind = "msg".into();
        assert!(e.validate().is_ok());
    }
    #[test]
    fn exclusive_binding_and_partial_lines() {
        let dir = std::env::temp_dir().join(format!("cybar-ipc-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.sock");
        let mut server = Server::bind(&path).unwrap();
        assert!(Server::bind(&path).is_err());
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .write_all(b"{\"id\":3,\"type\":\"request\",")
            .unwrap();
        assert!(server.pump().is_empty());
        client.write_all(b"\"method\":\"reload\"}\n").unwrap();
        let requests = server.pump();
        assert_eq!(requests.len(), 1);
        server.reply(
            requests[0].client,
            &requests[0].envelope,
            Ok(Value::Bool(true)),
        );
        server.flush();
        let mut data = [0u8; 128];
        let n = client.read(&mut data).unwrap();
        assert!(String::from_utf8_lossy(&data[..n]).contains("\"result\":true"));
        drop(server);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn half_closed_request_receives_reply() {
        let dir =
            std::env::temp_dir().join(format!("cybar-half-close-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.sock");
        let mut server = Server::bind(&path).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .write_all(b"{\"id\":7,\"type\":\"request\",\"method\":\"reload\"}\n")
            .unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let requests = server.pump();
        assert_eq!(requests.len(), 1);
        server.reply(
            requests[0].client,
            &requests[0].envelope,
            Ok(Value::Bool(true)),
        );
        server.flush();
        let mut data = String::new();
        client.read_to_string(&mut data).unwrap();
        assert!(data.contains("\"result\":true"));
        drop(server);
        let _ = fs::remove_dir_all(dir);
    }
}
