//! A small SFTP (protocol version 3) client.
//!
//! It talks to the server's `sftp` subsystem through `ssh -s`, multiplexed
//! over the interactive session's ControlMaster socket, so it never logs in
//! on its own: no second password prompt, and it shares the session's proxy.

use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};

const VERSION: u32 = 3;

// Packet types.
const FXP_INIT: u8 = 1;
const FXP_VERSION: u8 = 2;
const FXP_OPEN: u8 = 3;
const FXP_CLOSE: u8 = 4;
const FXP_READ: u8 = 5;
const FXP_WRITE: u8 = 6;
const FXP_LSTAT: u8 = 7;
const FXP_OPENDIR: u8 = 11;
const FXP_READDIR: u8 = 12;
const FXP_REMOVE: u8 = 13;
const FXP_MKDIR: u8 = 14;
const FXP_RMDIR: u8 = 15;
const FXP_REALPATH: u8 = 16;
const FXP_STAT: u8 = 17;
const FXP_RENAME: u8 = 18;
const FXP_STATUS: u8 = 101;
const FXP_HANDLE: u8 = 102;
const FXP_DATA: u8 = 103;
const FXP_NAME: u8 = 104;
const FXP_ATTRS: u8 = 105;

// Attribute flags.
const ATTR_SIZE: u32 = 0x1;
const ATTR_UIDGID: u32 = 0x2;
const ATTR_PERMISSIONS: u32 = 0x4;
const ATTR_ACMODTIME: u32 = 0x8;
const ATTR_EXTENDED: u32 = 0x8000_0000;

// Open flags.
const OPEN_READ: u32 = 0x1;
const OPEN_WRITE: u32 = 0x2;
const OPEN_CREAT: u32 = 0x8;
const OPEN_TRUNC: u32 = 0x10;
const OPEN_EXCL: u32 = 0x20;

// Status codes.
pub const STATUS_EOF: u32 = 1;
pub const STATUS_NO_SUCH_FILE: u32 = 2;
pub const STATUS_PERMISSION_DENIED: u32 = 3;

// File type bits of `permissions`.
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

/// Bytes per READ/WRITE request; every server accepts 32 KiB.
const CHUNK: u32 = 32 * 1024;
/// Requests kept in flight during a transfer.
const IN_FLIGHT: usize = 32;
/// Refuse absurd packet lengths instead of allocating them.
const MAX_PACKET: u32 = 4 * 1024 * 1024;

/// File attributes. Every field is optional in the protocol.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attrs {
    pub size: Option<u64>,
    pub uid_gid: Option<(u32, u32)>,
    pub permissions: Option<u32>,
    pub times: Option<(u32, u32)>,
}

impl Attrs {
    fn kind(&self) -> Option<u32> {
        self.permissions.map(|p| p & S_IFMT)
    }

    pub fn is_dir(&self) -> bool {
        self.kind() == Some(S_IFDIR)
    }

    pub fn is_symlink(&self) -> bool {
        self.kind() == Some(S_IFLNK)
    }

    pub fn is_file(&self) -> bool {
        self.kind() == Some(S_IFREG)
    }

    pub fn mtime(&self) -> Option<u32> {
        self.times.map(|(_, mtime)| mtime)
    }

    fn with_permissions(mode: u32) -> Attrs {
        Attrs {
            permissions: Some(mode),
            ..Attrs::default()
        }
    }
}

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub attrs: Attrs,
}

/// A failure reported by the server (`SSH_FXP_STATUS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusError {
    pub code: u32,
    pub message: String,
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self.code {
            STATUS_EOF => "end of file",
            STATUS_NO_SUCH_FILE => "no such file or folder",
            STATUS_PERMISSION_DENIED => "permission denied",
            5 => "bad message",
            6 => "no connection",
            7 => "connection lost",
            8 => "operation not supported",
            _ => "",
        };
        let message = self.message.trim();
        match (text, message) {
            ("", "") => write!(f, "failed (code {})", self.code),
            ("", message) => f.write_str(message),
            (text, _) => f.write_str(text),
        }
    }
}

impl std::error::Error for StatusError {}

/// The transfer was stopped through its progress callback.
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// An open remote file or directory.
pub struct Handle(Vec<u8>);

enum Reply {
    Status(StatusError),
    Handle(Vec<u8>),
    Data(Vec<u8>),
    Name(Vec<DirEntry>),
    Attrs(Attrs),
}

impl Reply {
    fn kind(&self) -> &'static str {
        match self {
            Reply::Status(_) => "status",
            Reply::Handle(_) => "handle",
            Reply::Data(_) => "data",
            Reply::Name(_) => "name",
            Reply::Attrs(_) => "attrs",
        }
    }
}

/// Builds one request packet.
struct Packet(Vec<u8>);

impl Packet {
    fn new(kind: u8, id: u32) -> Packet {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&[0; 4]);
        buf.push(kind);
        buf.extend_from_slice(&id.to_be_bytes());
        Packet(buf)
    }

    fn u32(mut self, value: u32) -> Packet {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn u64(mut self, value: u64) -> Packet {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn bytes(mut self, value: &[u8]) -> Packet {
        self.0
            .extend_from_slice(&(value.len() as u32).to_be_bytes());
        self.0.extend_from_slice(value);
        self
    }

    fn str(self, value: &str) -> Packet {
        self.bytes(value.as_bytes())
    }

    fn attrs(mut self, attrs: &Attrs) -> Packet {
        let mut flags = 0;
        if attrs.size.is_some() {
            flags |= ATTR_SIZE;
        }
        if attrs.uid_gid.is_some() {
            flags |= ATTR_UIDGID;
        }
        if attrs.permissions.is_some() {
            flags |= ATTR_PERMISSIONS;
        }
        if attrs.times.is_some() {
            flags |= ATTR_ACMODTIME;
        }
        self = self.u32(flags);
        if let Some(size) = attrs.size {
            self = self.u64(size);
        }
        if let Some((uid, gid)) = attrs.uid_gid {
            self = self.u32(uid).u32(gid);
        }
        if let Some(perm) = attrs.permissions {
            self = self.u32(perm);
        }
        if let Some((atime, mtime)) = attrs.times {
            self = self.u32(atime).u32(mtime);
        }
        self
    }

    fn finish(mut self) -> Vec<u8> {
        let len = (self.0.len() - 4) as u32;
        self.0[..4].copy_from_slice(&len.to_be_bytes());
        self.0
    }
}

/// Reads fields out of a reply payload.
struct Fields<'a> {
    buf: &'a [u8],
}

impl<'a> Fields<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            bail!("sftp: truncated reply");
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }

    fn attrs(&mut self) -> Result<Attrs> {
        let flags = self.u32()?;
        let mut attrs = Attrs::default();
        if flags & ATTR_SIZE != 0 {
            attrs.size = Some(self.u64()?);
        }
        if flags & ATTR_UIDGID != 0 {
            attrs.uid_gid = Some((self.u32()?, self.u32()?));
        }
        if flags & ATTR_PERMISSIONS != 0 {
            attrs.permissions = Some(self.u32()?);
        }
        if flags & ATTR_ACMODTIME != 0 {
            attrs.times = Some((self.u32()?, self.u32()?));
        }
        if flags & ATTR_EXTENDED != 0 {
            let count = self.u32()?;
            for _ in 0..count {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Ok(attrs)
    }
}

fn parse_reply(kind: u8, payload: &[u8]) -> Result<(u32, Reply)> {
    let mut f = Fields { buf: payload };
    let id = f.u32()?;
    let reply = match kind {
        FXP_STATUS => {
            let code = f.u32()?;
            // Version 3 servers may omit the message.
            let message = f.string().unwrap_or_default();
            Reply::Status(StatusError { code, message })
        }
        FXP_HANDLE => Reply::Handle(f.bytes()?.to_vec()),
        FXP_DATA => Reply::Data(f.bytes()?.to_vec()),
        FXP_NAME => {
            let count = f.u32()?;
            let mut entries = Vec::with_capacity(count.min(4096) as usize);
            for _ in 0..count {
                let name = f.string()?;
                let _long_name = f.bytes()?;
                let attrs = f.attrs()?;
                entries.push(DirEntry { name, attrs });
            }
            Reply::Name(entries)
        }
        FXP_ATTRS => Reply::Attrs(f.attrs()?),
        other => bail!("sftp: unexpected reply type {other}"),
    };
    Ok((id, reply))
}

/// Pass a status through as an error unless it is OK.
fn check(reply: Reply) -> Result<()> {
    match reply {
        Reply::Status(s) if s.code == 0 => Ok(()),
        Reply::Status(s) => Err(s.into()),
        other => bail!("sftp: expected status, got {}", other.kind()),
    }
}

/// A connected SFTP session.
pub struct Sftp {
    child: Child,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
    stderr: Arc<Mutex<String>>,
    next_id: u32,
}

impl Sftp {
    /// Start the sftp subsystem over the ControlMaster at `control_path`.
    ///
    /// The master must already be up. `ProxyCommand=false` makes sure ssh
    /// fails instead of opening a fresh connection (and asking for a
    /// password) if it is not.
    pub fn connect(target: &str, control_path: &Path) -> Result<Sftp> {
        if !control_path.exists() {
            bail!("the session is not logged in yet");
        }
        let mut child = Command::new("ssh")
            .arg("-o")
            .arg(format!("ControlPath={}", control_path.display()))
            .args([
                "-o",
                "ControlMaster=no",
                "-o",
                "BatchMode=yes",
                "-o",
                "ProxyCommand=false",
                "-o",
                "LogLevel=ERROR",
                "-T",
                "-x",
                "-a",
                "-s",
            ])
            .arg(target)
            .arg("sftp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to run ssh for sftp")?;

        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let sink = Arc::clone(&stderr);
            thread::spawn(move || {
                let mut text = String::new();
                let _ = pipe.read_to_string(&mut text);
                if let Ok(mut s) = sink.lock() {
                    *s = text;
                }
            });
        }
        let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            bail!("failed to talk to ssh");
        };
        let mut sftp = Sftp {
            child,
            input: BufWriter::with_capacity(256 * 1024, input),
            output: BufReader::with_capacity(256 * 1024, output),
            stderr,
            next_id: 1,
        };
        if let Err(e) = sftp.init() {
            let detail = sftp.ssh_error();
            return Err(match detail {
                Some(detail) => anyhow!("{detail}"),
                None => e,
            });
        }
        Ok(sftp)
    }

    fn init(&mut self) -> Result<()> {
        let mut packet = Vec::with_capacity(9);
        packet.extend_from_slice(&5u32.to_be_bytes());
        packet.push(FXP_INIT);
        packet.extend_from_slice(&VERSION.to_be_bytes());
        self.input.write_all(&packet)?;
        self.input.flush()?;
        let (kind, payload) = self.recv_raw().context("the server has no sftp service")?;
        if kind != FXP_VERSION {
            bail!("sftp: unexpected reply type {kind} to init");
        }
        let version = Fields { buf: &payload }.u32()?;
        if version < VERSION {
            bail!("the server only speaks sftp version {version}");
        }
        Ok(())
    }

    /// What ssh printed on stderr, once it has exited.
    fn ssh_error(&mut self) -> Option<String> {
        let _ = self.child.wait();
        // Give the stderr reader a moment to store the text.
        for _ in 0..20 {
            if let Ok(s) = self.stderr.lock()
                && !s.trim().is_empty()
            {
                return Some(s.trim().lines().last().unwrap_or("").to_string());
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn send(&mut self, packet: Packet) -> Result<()> {
        self.input
            .write_all(&packet.finish())
            .context("sftp connection lost")
    }

    fn recv_raw(&mut self) -> Result<(u8, Vec<u8>)> {
        let mut len = [0u8; 4];
        self.input.flush().context("sftp connection lost")?;
        self.output
            .read_exact(&mut len)
            .context("sftp connection lost")?;
        let len = u32::from_be_bytes(len);
        if len == 0 || len > MAX_PACKET {
            bail!("sftp: bad packet length {len}");
        }
        let mut buf = vec![0u8; len as usize];
        self.output
            .read_exact(&mut buf)
            .context("sftp connection lost")?;
        let kind = buf[0];
        buf.remove(0);
        Ok((kind, buf))
    }

    fn recv(&mut self) -> Result<(u32, Reply)> {
        let (kind, payload) = self.recv_raw()?;
        parse_reply(kind, &payload)
    }

    /// Send one request and wait for its reply.
    fn call(&mut self, packet: Packet, id: u32) -> Result<Reply> {
        self.send(packet)?;
        let (got, reply) = self.recv()?;
        if got != id {
            bail!("sftp: reply for request {got}, expected {id}");
        }
        Ok(reply)
    }

    pub fn realpath(&mut self, path: &str) -> Result<String> {
        let id = self.id();
        match self.call(Packet::new(FXP_REALPATH, id).str(path), id)? {
            Reply::Name(mut names) if !names.is_empty() => Ok(names.swap_remove(0).name),
            Reply::Status(s) => Err(s.into()),
            other => bail!("sftp: expected name, got {}", other.kind()),
        }
    }

    fn stat_kind(&mut self, kind: u8, path: &str) -> Result<Attrs> {
        let id = self.id();
        match self.call(Packet::new(kind, id).str(path), id)? {
            Reply::Attrs(attrs) => Ok(attrs),
            Reply::Status(s) => Err(s.into()),
            other => bail!("sftp: expected attrs, got {}", other.kind()),
        }
    }

    /// Attributes, following symlinks.
    pub fn stat(&mut self, path: &str) -> Result<Attrs> {
        self.stat_kind(FXP_STAT, path)
    }

    /// Attributes of the path itself, not a symlink's target.
    pub fn lstat(&mut self, path: &str) -> Result<Attrs> {
        self.stat_kind(FXP_LSTAT, path)
    }

    fn handle_reply(reply: Reply) -> Result<Handle> {
        match reply {
            Reply::Handle(handle) => Ok(Handle(handle)),
            Reply::Status(s) => Err(s.into()),
            other => bail!("sftp: expected handle, got {}", other.kind()),
        }
    }

    pub fn close(&mut self, handle: Handle) -> Result<()> {
        let id = self.id();
        check(self.call(Packet::new(FXP_CLOSE, id).bytes(&handle.0), id)?)
    }

    /// Every entry of a directory except `.` and `..`.
    pub fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let id = self.id();
        let handle = Self::handle_reply(self.call(Packet::new(FXP_OPENDIR, id).str(path), id)?)?;
        let mut entries = Vec::new();
        let result = loop {
            let id = self.id();
            match self.call(Packet::new(FXP_READDIR, id).bytes(&handle.0), id) {
                Ok(Reply::Name(names)) => entries.extend(
                    names
                        .into_iter()
                        .filter(|e| e.name != "." && e.name != ".."),
                ),
                Ok(Reply::Status(s)) if s.code == STATUS_EOF => break Ok(()),
                Ok(Reply::Status(s)) => break Err(s.into()),
                Ok(other) => break Err(anyhow!("sftp: expected name, got {}", other.kind())),
                Err(e) => return Err(e),
            }
        };
        let closed = self.close(handle);
        result?;
        closed?;
        Ok(entries)
    }

    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        let id = self.id();
        check(
            self.call(
                Packet::new(FXP_MKDIR, id)
                    .str(path)
                    .attrs(&Attrs::with_permissions(0o755)),
                id,
            )?,
        )
    }

    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        let id = self.id();
        check(self.call(Packet::new(FXP_RMDIR, id).str(path), id)?)
    }

    pub fn remove(&mut self, path: &str) -> Result<()> {
        let id = self.id();
        check(self.call(Packet::new(FXP_REMOVE, id).str(path), id)?)
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let id = self.id();
        check(self.call(Packet::new(FXP_RENAME, id).str(from).str(to), id)?)
    }

    fn open(&mut self, path: &str, flags: u32, attrs: &Attrs) -> Result<Handle> {
        let id = self.id();
        Self::handle_reply(self.call(
            Packet::new(FXP_OPEN, id).str(path).u32(flags).attrs(attrs),
            id,
        )?)
    }

    /// Create an empty file; fails if something with that name exists.
    pub fn create_new(&mut self, path: &str) -> Result<()> {
        let handle = self.open(
            path,
            OPEN_WRITE | OPEN_CREAT | OPEN_EXCL,
            &Attrs::with_permissions(0o644),
        )?;
        self.close(handle)
    }

    /// Copy the remote file `remote` into `local`, calling `progress` with
    /// the bytes received so far; it returns false to cancel.
    /// `size` is the expected size, used to read ahead; the copy still
    /// runs to the real end of the file.
    pub fn download(
        &mut self,
        remote: &str,
        size: u64,
        local: &File,
        progress: &mut dyn FnMut(u64) -> bool,
    ) -> Result<()> {
        let handle = self.open(remote, OPEN_READ, &Attrs::default())?;
        let result = self.read_into(&handle, size, local, progress);
        let closed = self.close(handle);
        result?;
        closed
    }

    fn read_into(
        &mut self,
        handle: &Handle,
        size: u64,
        local: &File,
        progress: &mut dyn FnMut(u64) -> bool,
    ) -> Result<()> {
        // (id, offset, length) of each READ in flight.
        let mut pending: VecDeque<(u32, u64, u32)> = VecDeque::new();
        let mut next = 0u64;
        let mut eof = false;
        let mut done = 0u64;
        let mut failure: Option<anyhow::Error> = None;

        loop {
            // Read ahead up to the expected size; past it (or for a file of
            // unknown size) one request at a time finds the real end.
            while failure.is_none()
                && !eof
                && pending.len() < IN_FLIGHT
                && (next < size || pending.is_empty())
            {
                let id = self.id();
                self.send(
                    Packet::new(FXP_READ, id)
                        .bytes(&handle.0)
                        .u64(next)
                        .u32(CHUNK),
                )?;
                pending.push_back((id, next, CHUNK));
                next += u64::from(CHUNK);
            }
            if pending.is_empty() {
                break;
            }
            let (id, reply) = self.recv()?;
            let Some(at) = pending.iter().position(|p| p.0 == id) else {
                bail!("sftp: reply for unknown request {id}");
            };
            let (_, offset, len) = pending.remove(at).unwrap_or_default();
            if failure.is_some() {
                // Only draining replies so the stream stays in sync.
                continue;
            }
            match reply {
                Reply::Data(data) if data.is_empty() => eof = true,
                Reply::Data(data) => {
                    if let Err(e) = local.write_all_at(&data, offset) {
                        failure = Some(anyhow!(e).context("failed to write the local file"));
                        continue;
                    }
                    done += data.len() as u64;
                    let got = data.len() as u32;
                    if got < len {
                        // Short read: ask for the rest of this block.
                        let id = self.id();
                        let rest = offset + u64::from(got);
                        self.send(
                            Packet::new(FXP_READ, id)
                                .bytes(&handle.0)
                                .u64(rest)
                                .u32(len - got),
                        )?;
                        pending.push_back((id, rest, len - got));
                    }
                    if !progress(done) {
                        failure = Some(Cancelled.into());
                    }
                }
                Reply::Status(s) if s.code == STATUS_EOF => eof = true,
                Reply::Status(s) => failure = Some(s.into()),
                other => failure = Some(anyhow!("sftp: expected data, got {}", other.kind())),
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Copy the local file `local` (of `size` bytes) to `remote`, creating
    /// or truncating it with mode `mode`.
    pub fn upload(
        &mut self,
        local: &mut File,
        size: u64,
        remote: &str,
        mode: u32,
        progress: &mut dyn FnMut(u64) -> bool,
    ) -> Result<()> {
        let handle = self.open(
            remote,
            OPEN_WRITE | OPEN_CREAT | OPEN_TRUNC,
            &Attrs::with_permissions(mode & 0o777),
        )?;
        let result = self.write_from(&handle, size, local, progress);
        let closed = self.close(handle);
        result?;
        closed
    }

    fn write_from(
        &mut self,
        handle: &Handle,
        size: u64,
        local: &mut File,
        progress: &mut dyn FnMut(u64) -> bool,
    ) -> Result<()> {
        let mut pending: VecDeque<(u32, u64)> = VecDeque::new();
        let mut offset = 0u64;
        let mut done = 0u64;
        let mut finished = false;
        let mut failure: Option<anyhow::Error> = None;
        let mut buf = vec![0u8; CHUNK as usize];

        loop {
            while failure.is_none() && !finished && pending.len() < IN_FLIGHT {
                let n = match read_full(local, &mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        failure = Some(anyhow!(e).context("failed to read the local file"));
                        break;
                    }
                };
                if n == 0 {
                    finished = true;
                    break;
                }
                let id = self.id();
                self.send(
                    Packet::new(FXP_WRITE, id)
                        .bytes(&handle.0)
                        .u64(offset)
                        .bytes(&buf[..n]),
                )?;
                pending.push_back((id, n as u64));
                offset += n as u64;
                // The file may grow while it is copied; stop at its size
                // when the copy started.
                if offset >= size && size > 0 {
                    finished = true;
                }
            }
            if pending.is_empty() {
                break;
            }
            let (id, reply) = self.recv()?;
            let Some(at) = pending.iter().position(|p| p.0 == id) else {
                bail!("sftp: reply for unknown request {id}");
            };
            let (_, len) = pending.remove(at).unwrap_or_default();
            if failure.is_some() {
                continue;
            }
            match check(reply) {
                Ok(()) => {
                    done += len;
                    if !progress(done) {
                        failure = Some(Cancelled.into());
                    }
                }
                Err(e) => failure = Some(e),
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Fill `buf` as far as the file allows; 0 means end of file.
fn read_full(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

impl Drop for Sftp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Join a remote directory and a name.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The parent of a remote path (`/` stays `/`).
pub fn parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
    }
}

/// The last component of a remote path.
pub fn file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_are_length_prefixed() {
        let p = Packet::new(FXP_REALPATH, 7).str(".").finish();
        assert_eq!(p, [0, 0, 0, 10, 16, 0, 0, 0, 7, 0, 0, 0, 1, b'.']);
    }

    #[test]
    fn attrs_roundtrip() {
        let attrs = Attrs {
            size: Some(1234),
            uid_gid: Some((1000, 100)),
            permissions: Some(0o100644),
            times: Some((1, 2)),
        };
        let packet = Packet::new(FXP_ATTRS, 3).attrs(&attrs).finish();
        let (id, reply) = parse_reply(packet[4], &packet[5..]).unwrap();
        assert_eq!(id, 3);
        match reply {
            Reply::Attrs(parsed) => {
                assert_eq!(parsed, attrs);
                assert!(parsed.is_file() && !parsed.is_dir());
            }
            _ => panic!("expected attrs"),
        }
    }

    #[test]
    fn name_replies_parse() {
        let mut payload = Packet::new(FXP_NAME, 9).u32(2);
        for (name, mode) in [("docs", 0o040755), ("a.txt", 0o100600)] {
            payload = payload
                .str(name)
                .str("long name")
                .attrs(&Attrs::with_permissions(mode));
        }
        let packet = payload.finish();
        let (_, reply) = parse_reply(packet[4], &packet[5..]).unwrap();
        let Reply::Name(entries) = reply else {
            panic!("expected name");
        };
        assert_eq!(entries.len(), 2);
        assert!(entries[0].attrs.is_dir());
        assert_eq!(entries[1].name, "a.txt");
    }

    #[test]
    fn status_without_message() {
        let packet = Packet::new(FXP_STATUS, 1).u32(STATUS_EOF).finish();
        let (_, reply) = parse_reply(packet[4], &packet[5..]).unwrap();
        let Reply::Status(s) = reply else {
            panic!("expected status");
        };
        assert_eq!(s.code, STATUS_EOF);
        assert_eq!(
            StatusError {
                code: 3,
                message: "Permission denied".into()
            }
            .to_string(),
            "permission denied"
        );
    }

    #[test]
    fn truncated_replies_are_errors() {
        assert!(parse_reply(FXP_HANDLE, &[0, 0, 0, 1, 0, 0, 0, 9, 1]).is_err());
    }

    #[test]
    fn path_helpers() {
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(join("/home/a", "b"), "/home/a/b");
        assert_eq!(parent("/home/a"), "/home");
        assert_eq!(parent("/home"), "/");
        assert_eq!(parent("/"), "/");
        assert_eq!(file_name("/home/a/"), "a");
        assert_eq!(file_name("/"), "");
    }
}
