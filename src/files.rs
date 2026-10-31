//! Background workers for the file panel.
//!
//! Each worker owns one SFTP connection and runs its tasks in order, so the
//! panel never blocks on the network. The panel uses two: one for quick
//! things (listing, creating, renaming, scanning) and one for transfers and
//! deletes, so browsing stays responsive while a big upload runs.

use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::sftp::{self, Cancelled, DirEntry, Sftp, StatusError};

/// What a directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    /// A symbolic link; `true` when it points at a directory.
    Link(bool),
    Other,
}

/// One row of a remote directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    /// Permission bits (including the file type bits), if known.
    pub mode: Option<u32>,
    pub mtime: Option<u32>,
}

impl Entry {
    /// Directories and links to directories can be opened.
    pub fn is_dir_like(&self) -> bool {
        matches!(self.kind, Kind::Dir | Kind::Link(true))
    }

    pub fn is_hidden(&self) -> bool {
        self.name.starts_with('.')
    }
}

/// Totals of a recursive scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

/// How far a transfer or delete has come.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub bytes: u64,
    pub total_bytes: u64,
    pub files: u64,
    pub total_files: u64,
    /// Name of the file being worked on.
    pub current: String,
}

/// Work for a [`Worker`].
pub enum Task {
    List {
        seq: u64,
        path: String,
        select: Option<String>,
    },
    Mkdir {
        dir: String,
        name: String,
    },
    Touch {
        dir: String,
        name: String,
    },
    Rename {
        dir: String,
        from: String,
        to: String,
    },
    /// Count what is in `paths`. With `follow`, a link given directly is
    /// measured by what it points to; links inside folders never are.
    Scan {
        id: u64,
        paths: Vec<String>,
        follow: bool,
    },
    Upload {
        id: u64,
        sources: Vec<PathBuf>,
        dir: String,
    },
    Download {
        id: u64,
        sources: Vec<String>,
        dir: PathBuf,
    },
    Delete {
        id: u64,
        paths: Vec<String>,
    },
    /// Close the connection once the tasks before this one are done (it
    /// reopens on the next task).
    Disconnect,
}

/// Results sent back to the panel.
#[derive(Debug)]
pub enum Event {
    /// The worker is waiting for the session to log in.
    Waiting,
    Listed {
        seq: u64,
        path: String,
        select: Option<String>,
        result: Result<Vec<Entry>, String>,
    },
    /// A quick change (mkdir, touch, rename) in `dir` finished.
    Changed {
        dir: String,
        select: Option<String>,
        result: Result<String, String>,
    },
    Scanned {
        id: u64,
        result: Result<Summary, String>,
    },
    Started {
        id: u64,
    },
    Progress {
        id: u64,
        progress: Progress,
    },
    /// A job finished; `dir` is the remote directory it changed, if any.
    Finished {
        id: u64,
        dir: Option<String>,
        result: Result<String, String>,
    },
}

/// A worker thread with its own SFTP connection.
pub struct Worker {
    tx: Sender<Task>,
    cancelled: Arc<Mutex<HashSet<u64>>>,
}

impl Worker {
    pub fn spawn(target: String, control_path: PathBuf, events: Sender<Event>) -> Worker {
        let (tx, rx) = mpsc::channel();
        let cancelled = Arc::new(Mutex::new(HashSet::new()));
        let state = Runner {
            target,
            control_path,
            events,
            cancelled: Arc::clone(&cancelled),
            conn: None,
        };
        thread::spawn(move || state.run(rx));
        Worker { tx, cancelled }
    }

    pub fn send(&self, task: Task) {
        let _ = self.tx.send(task);
    }

    /// Stop the task with this id (now, or before it starts).
    pub fn cancel(&self, id: u64) {
        if let Ok(mut set) = self.cancelled.lock() {
            set.insert(id);
        }
    }

    pub fn disconnect(&self) {
        self.send(Task::Disconnect);
    }
}

struct Runner {
    target: String,
    control_path: PathBuf,
    events: Sender<Event>,
    cancelled: Arc<Mutex<HashSet<u64>>>,
    conn: Option<Sftp>,
}

/// How long a task waits for the session to log in.
const LOGIN_WAIT: Duration = Duration::from_secs(120);

impl Runner {
    fn run(mut self, rx: Receiver<Task>) {
        while let Ok(task) = rx.recv() {
            if !self.handle(task) {
                break;
            }
        }
    }

    fn is_cancelled(&self, id: u64) -> bool {
        self.cancelled.lock().is_ok_and(|set| set.contains(&id))
    }

    fn forget(&self, id: u64) {
        if let Ok(mut set) = self.cancelled.lock() {
            set.remove(&id);
        }
    }

    /// The connection, opening it (after the session logs in) if needed.
    fn sftp(&mut self) -> Result<&mut Sftp> {
        if self.conn.is_none() {
            if !self.control_path.exists() {
                let _ = self.events.send(Event::Waiting);
                let deadline = Instant::now() + LOGIN_WAIT;
                while !self.control_path.exists() {
                    if Instant::now() > deadline {
                        bail!("the session did not log in");
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            }
            self.conn = Some(Sftp::connect(&self.target, &self.control_path)?);
        }
        Ok(self.conn.as_mut().expect("connection was just opened"))
    }

    /// Drop the connection after errors that are not plain server replies,
    /// so the next task starts with a fresh one.
    fn check_conn(&mut self, e: &anyhow::Error) {
        if e.downcast_ref::<StatusError>().is_none() && e.downcast_ref::<Cancelled>().is_none() {
            self.conn = None;
        }
    }

    /// Returns false once the panel is gone.
    fn handle(&mut self, task: Task) -> bool {
        let event = match task {
            Task::List { seq, path, select } => {
                let result = self.list(&path);
                let (path, result) = match result {
                    Ok((path, entries)) => (path, Ok(entries)),
                    Err(e) => {
                        self.check_conn(&e);
                        (path, Err(message(&e)))
                    }
                };
                Event::Listed {
                    seq,
                    path,
                    select,
                    result,
                }
            }
            Task::Mkdir { dir, name } => {
                let path = sftp::join(&dir, &name);
                let result = self
                    .sftp()
                    .and_then(|s| s.mkdir(&path))
                    .map(|()| format!("created folder {name}"));
                self.changed(dir, name, result)
            }
            Task::Touch { dir, name } => {
                let path = sftp::join(&dir, &name);
                let result = self
                    .sftp()
                    .and_then(|s| s.create_new(&path))
                    .map(|()| format!("created file {name}"));
                self.changed(dir, name, result)
            }
            Task::Rename { dir, from, to } => {
                let result = self.rename(&dir, &from, &to);
                self.changed(dir, to, result)
            }
            Task::Scan { id, paths, follow } => {
                let result = if self.is_cancelled(id) {
                    Err(anyhow!(Cancelled))
                } else {
                    self.scan(id, &paths, follow)
                };
                self.forget(id);
                Event::Scanned {
                    id,
                    result: result.map_err(|e| {
                        self.check_conn(&e);
                        message(&e)
                    }),
                }
            }
            Task::Upload { id, sources, dir } => {
                let result = self.job(id, |r| r.upload(id, &sources, &dir));
                Event::Finished {
                    id,
                    dir: Some(dir),
                    result,
                }
            }
            Task::Download { id, sources, dir } => {
                let result = self.job(id, |r| r.download(id, &sources, &dir));
                Event::Finished {
                    id,
                    dir: None,
                    result,
                }
            }
            Task::Delete { id, paths } => {
                let dir = paths.first().map(|p| sftp::parent(p));
                let result = self.job(id, |r| r.delete(id, &paths));
                Event::Finished { id, dir, result }
            }
            Task::Disconnect => {
                self.conn = None;
                return true;
            }
        };
        self.events.send(event).is_ok()
    }

    fn changed(&mut self, dir: String, name: String, result: Result<String>) -> Event {
        Event::Changed {
            dir,
            select: Some(name),
            result: result.map_err(|e| {
                self.check_conn(&e);
                message(&e)
            }),
        }
    }

    fn job(
        &mut self,
        id: u64,
        f: impl FnOnce(&mut Self) -> Result<String>,
    ) -> Result<String, String> {
        let result = if self.is_cancelled(id) {
            Err(anyhow!(Cancelled))
        } else {
            let _ = self.events.send(Event::Started { id });
            f(self)
        };
        self.forget(id);
        result.map_err(|e| {
            self.check_conn(&e);
            message(&e)
        })
    }

    fn list(&mut self, path: &str) -> Result<(String, Vec<Entry>)> {
        let sftp = self.sftp()?;
        let path = sftp.realpath(if path.is_empty() { "." } else { path })?;
        let raw = sftp.read_dir(&path)?;
        let mut entries = Vec::with_capacity(raw.len());
        for DirEntry { name, attrs } in raw {
            let kind = if attrs.is_dir() {
                Kind::Dir
            } else if attrs.is_symlink() {
                // Follow the link to tell folders from files.
                let target = sftp.stat(&sftp::join(&path, &name)).ok();
                Kind::Link(target.is_some_and(|t| t.is_dir()))
            } else if attrs.is_file() || attrs.permissions.is_none() {
                Kind::File
            } else {
                Kind::Other
            };
            entries.push(Entry {
                name,
                kind,
                size: attrs.size.unwrap_or(0),
                mode: attrs.permissions,
                mtime: attrs.mtime(),
            });
        }
        Ok((path, entries))
    }

    fn rename(&mut self, dir: &str, from: &str, to: &str) -> Result<String> {
        let sftp = self.sftp()?;
        let target = sftp::join(dir, to);
        // SFTP v3 rename never overwrites on OpenSSH, but other servers may.
        if sftp.lstat(&target).is_ok() {
            bail!("{to} already exists");
        }
        sftp.rename(&sftp::join(dir, from), &target)?;
        Ok(format!("renamed {from} to {to}"))
    }

    fn scan(&mut self, id: u64, paths: &[String], follow: bool) -> Result<Summary> {
        let mut summary = Summary::default();
        for path in paths {
            let sftp = self.sftp()?;
            let attrs = if follow {
                sftp.stat(path)?
            } else {
                sftp.lstat(path)?
            };
            if attrs.is_dir() {
                summary.dirs += 1;
                self.scan_dir(id, path, &mut summary)?;
            } else {
                summary.files += 1;
                summary.bytes += attrs.size.unwrap_or(0);
            }
        }
        Ok(summary)
    }

    fn scan_dir(&mut self, id: u64, dir: &str, summary: &mut Summary) -> Result<()> {
        if self.is_cancelled(id) {
            return Err(Cancelled.into());
        }
        for entry in self.sftp()?.read_dir(dir)? {
            if entry.attrs.is_dir() {
                summary.dirs += 1;
                self.scan_dir(id, &sftp::join(dir, &entry.name), summary)?;
            } else {
                summary.files += 1;
                summary.bytes += entry.attrs.size.unwrap_or(0);
            }
        }
        Ok(())
    }

    fn upload(&mut self, id: u64, sources: &[PathBuf], dir: &str) -> Result<String> {
        let mut plan = Vec::new();
        let mut skipped = 0;
        for source in sources {
            let meta = fs::metadata(source)
                .with_context(|| format!("cannot read {}", source.display()))?;
            let name = source
                .file_name()
                .with_context(|| format!("{} has no file name", source.display()))?
                .to_string_lossy()
                .into_owned();
            let remote = sftp::join(dir, &name);
            if meta.is_dir() {
                plan.push(Item::dir(source.clone(), remote.clone()));
                plan_local(source, &remote, &mut plan, &mut skipped, 0)?;
            } else if meta.is_file() {
                plan.push(Item::file(
                    source.clone(),
                    remote,
                    meta.len(),
                    mode_of(&meta),
                ));
            } else {
                skipped += 1;
            }
        }

        let mut tracker = Tracker::new(id, &plan, self.events.clone(), Arc::clone(&self.cancelled));
        for item in &plan {
            if tracker.cancelled() {
                return Err(Cancelled.into());
            }
            let sftp = self.sftp()?;
            if item.is_dir {
                if let Err(e) = sftp.mkdir(&item.remote) {
                    // Fine if it is already there.
                    if !sftp.stat(&item.remote).is_ok_and(|a| a.is_dir()) {
                        return Err(e.context(format!("cannot create {}", item.remote)));
                    }
                }
                continue;
            }
            let mut file = File::open(&item.local)
                .with_context(|| format!("cannot open {}", item.local.display()))?;
            tracker.start(&item.remote, item.size);
            let copied = sftp.upload(&mut file, item.size, &item.remote, item.mode, &mut |done| {
                tracker.update(done)
            });
            if let Err(e) = copied {
                // Don't leave a half-written file on the server; the old
                // contents were already replaced. A lost connection can't
                // clean up, so the error is what counts.
                if e.downcast_ref::<StatusError>().is_some()
                    || e.downcast_ref::<Cancelled>().is_some()
                {
                    let _ = sftp.remove(&item.remote);
                }
                return Err(e.context(format!("uploading {}", sftp::file_name(&item.remote))));
            }
            tracker.finish_file(item.size);
        }
        Ok(done_message("uploaded", &tracker, skipped))
    }

    fn download(&mut self, id: u64, sources: &[String], dir: &Path) -> Result<String> {
        let mut plan = Vec::new();
        let mut skipped = 0;
        for source in sources {
            let attrs = self.sftp()?.stat(source)?;
            let local = dir.join(sftp::file_name(source));
            if attrs.is_dir() {
                plan.push(Item::dir(local.clone(), source.clone()));
                self.plan_remote(id, source, &local, &mut plan, &mut skipped, 0)?;
            } else if attrs.is_file() {
                plan.push(Item::file(
                    local,
                    source.clone(),
                    attrs.size.unwrap_or(0),
                    attrs.permissions.unwrap_or(0o644),
                ));
            } else {
                skipped += 1;
            }
        }

        let mut tracker = Tracker::new(id, &plan, self.events.clone(), Arc::clone(&self.cancelled));
        for item in &plan {
            if tracker.cancelled() {
                return Err(Cancelled.into());
            }
            if item.is_dir {
                fs::create_dir_all(&item.local)
                    .with_context(|| format!("cannot create {}", item.local.display()))?;
                continue;
            }
            let file = File::create(&item.local)
                .with_context(|| format!("cannot create {}", item.local.display()))?;
            tracker.start(&item.remote, item.size);
            let sftp = self.sftp()?;
            let copied = sftp.download(&item.remote, item.size, &file, &mut |done| {
                tracker.update(done)
            });
            if let Err(e) = copied {
                // Never leave a half-written file behind.
                drop(file);
                let _ = fs::remove_file(&item.local);
                return Err(e.context(format!("downloading {}", sftp::file_name(&item.remote))));
            }
            // Keep the executable bits, like scp does.
            let exec = item.mode & 0o111;
            if exec != 0 {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = file.metadata() {
                    let mode = meta.permissions().mode() | exec;
                    let _ = file.set_permissions(fs::Permissions::from_mode(mode));
                }
            }
            tracker.finish_file(item.size);
        }
        Ok(done_message("downloaded", &tracker, skipped))
    }

    fn plan_remote(
        &mut self,
        id: u64,
        dir: &str,
        local: &Path,
        plan: &mut Vec<Item>,
        skipped: &mut u64,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            bail!("{dir} is nested too deeply");
        }
        if self.is_cancelled(id) {
            return Err(Cancelled.into());
        }
        let entries = self.sftp()?.read_dir(dir)?;
        for entry in entries {
            let remote = sftp::join(dir, &entry.name);
            let target = local.join(&entry.name);
            let mut attrs = entry.attrs;
            if attrs.is_symlink() {
                // Links to files are copied; links to folders are skipped so
                // a loop can't recurse forever.
                match self.sftp()?.stat(&remote) {
                    Ok(followed) if followed.is_file() => attrs = followed,
                    _ => {
                        *skipped += 1;
                        continue;
                    }
                }
            }
            if attrs.is_dir() {
                plan.push(Item::dir(target.clone(), remote.clone()));
                self.plan_remote(id, &remote, &target, plan, skipped, depth + 1)?;
            } else if attrs.is_file() {
                plan.push(Item::file(
                    target,
                    remote,
                    attrs.size.unwrap_or(0),
                    attrs.permissions.unwrap_or(0o644),
                ));
            } else {
                *skipped += 1;
            }
        }
        Ok(())
    }

    fn delete(&mut self, id: u64, paths: &[String]) -> Result<String> {
        for path in paths {
            check_deletable(path)?;
        }
        let mut progress = Progress::default();
        let mut last = Instant::now();
        for path in paths {
            let attrs = self.sftp()?.lstat(path)?;
            if attrs.is_dir() {
                self.delete_dir(id, path, &mut progress, &mut last, 0)?;
            } else {
                self.sftp()?.remove(path)?;
                progress.files += 1;
            }
        }
        let names = match paths {
            [one] => sftp::file_name(one).to_string(),
            many => format!("{} items", many.len()),
        };
        Ok(format!("deleted {names}"))
    }

    fn delete_dir(
        &mut self,
        id: u64,
        dir: &str,
        progress: &mut Progress,
        last: &mut Instant,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            bail!("{dir} is nested too deeply");
        }
        if self.is_cancelled(id) {
            return Err(Cancelled.into());
        }
        let entries = self.sftp()?.read_dir(dir)?;
        for entry in entries {
            let path = sftp::join(dir, &entry.name);
            // Directory entries come with lstat attributes, so a link to a
            // folder is removed as a link, never followed.
            if entry.attrs.is_dir() {
                self.delete_dir(id, &path, progress, last, depth + 1)?;
            } else {
                self.sftp()?.remove(&path)?;
                progress.files += 1;
                if last.elapsed() >= PROGRESS_EVERY {
                    *last = Instant::now();
                    progress.current = entry.name.clone();
                    let _ = self.events.send(Event::Progress {
                        id,
                        progress: progress.clone(),
                    });
                }
            }
        }
        self.sftp()?.rmdir(dir)
    }
}

/// Folders deeper than this are refused (a safety net against loops).
const MAX_DEPTH: usize = 64;
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Refuse to delete paths that are obviously not meant: the root, empty
/// paths, and `.`/`..` components.
fn check_deletable(path: &str) -> Result<()> {
    let name = sftp::file_name(path);
    if path.trim_matches('/').is_empty() || name.is_empty() || name == "." || name == ".." {
        bail!("refusing to delete {path:?}");
    }
    if !path.starts_with('/') {
        bail!("refusing to delete a relative path ({path})");
    }
    Ok(())
}

/// One step of a transfer.
struct Item {
    local: PathBuf,
    remote: String,
    is_dir: bool,
    size: u64,
    mode: u32,
}

impl Item {
    fn dir(local: PathBuf, remote: String) -> Item {
        Item {
            local,
            remote,
            is_dir: true,
            size: 0,
            mode: 0o755,
        }
    }

    fn file(local: PathBuf, remote: String, size: u64, mode: u32) -> Item {
        Item {
            local,
            remote,
            is_dir: false,
            size,
            mode,
        }
    }
}

fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

/// Add everything inside the local folder `dir` to the upload plan.
fn plan_local(
    dir: &Path,
    remote: &str,
    plan: &mut Vec<Item>,
    skipped: &mut u64,
    depth: usize,
) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("{} is nested too deeply", dir.display());
    }
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let target = sftp::join(remote, &name);
        let Ok(kind) = entry.file_type() else {
            *skipped += 1;
            continue;
        };
        if kind.is_dir() {
            plan.push(Item::dir(path.clone(), target.clone()));
            plan_local(&path, &target, plan, skipped, depth + 1)?;
            continue;
        }
        // Follow links to files; skip links to folders (loops) and
        // anything that is not a regular file.
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                plan.push(Item::file(path, target, meta.len(), mode_of(&meta)));
            }
            _ => *skipped += 1,
        }
    }
    Ok(())
}

/// Reports transfer progress, throttled, and watches for cancellation.
struct Tracker {
    id: u64,
    events: Sender<Event>,
    cancelled: Arc<Mutex<HashSet<u64>>>,
    progress: Progress,
    /// Bytes of the files already finished.
    base: u64,
    last: Instant,
}

impl Tracker {
    fn new(
        id: u64,
        plan: &[Item],
        events: Sender<Event>,
        cancelled: Arc<Mutex<HashSet<u64>>>,
    ) -> Tracker {
        let files: Vec<&Item> = plan.iter().filter(|i| !i.is_dir).collect();
        let progress = Progress {
            total_bytes: files.iter().map(|i| i.size).sum(),
            total_files: files.len() as u64,
            ..Progress::default()
        };
        let tracker = Tracker {
            id,
            events,
            cancelled,
            progress,
            base: 0,
            last: Instant::now(),
        };
        tracker.send();
        tracker
    }

    fn cancelled(&self) -> bool {
        self.cancelled
            .lock()
            .is_ok_and(|set| set.contains(&self.id))
    }

    fn send(&self) {
        let _ = self.events.send(Event::Progress {
            id: self.id,
            progress: self.progress.clone(),
        });
    }

    fn start(&mut self, remote: &str, _size: u64) {
        self.progress.current = sftp::file_name(remote).to_string();
        self.send();
        self.last = Instant::now();
    }

    /// Called with the bytes of the current file; false cancels.
    fn update(&mut self, done: u64) -> bool {
        self.progress.bytes = self.base + done;
        if self.last.elapsed() >= PROGRESS_EVERY {
            self.last = Instant::now();
            self.send();
        }
        !self.cancelled()
    }

    fn finish_file(&mut self, size: u64) {
        self.base += size;
        self.progress.bytes = self.base;
        self.progress.files += 1;
        if self.last.elapsed() >= PROGRESS_EVERY || self.progress.files == self.progress.total_files
        {
            self.last = Instant::now();
            self.send();
        }
    }
}

fn done_message(verb: &str, tracker: &Tracker, skipped: u64) -> String {
    let p = &tracker.progress;
    let mut text = match p.total_files {
        1 => format!("{verb} {} ({})", p.current, human_size(p.bytes)),
        n => format!("{verb} {n} files ({})", human_size(p.bytes)),
    };
    if skipped > 0 {
        text.push_str(&format!(", skipped {skipped}"));
    }
    text
}

/// A short, readable error for the panel's status line.
fn message(e: &anyhow::Error) -> String {
    if e.downcast_ref::<Cancelled>().is_some() {
        return "cancelled".to_string();
    }
    let chain: Vec<String> = e.chain().map(|c| c.to_string()).collect();
    chain.join(": ")
}

/// Size with a binary unit, like `ls -h`: 512B, 4.0K, 12M, 1.5G.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{}{}", value.round() as u64, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_human() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(1023), "1023B");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(20 * 1024), "20K");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0G");
    }

    #[test]
    fn dangerous_deletes_are_refused() {
        for path in ["/", "", "//", "/home/..", "/tmp/.", "relative/x"] {
            assert!(check_deletable(path).is_err(), "{path:?}");
        }
        assert!(check_deletable("/home/me/old").is_ok());
    }

    #[test]
    fn local_plan_walks_folders() {
        let dir = std::env::temp_dir().join(format!("lssh-plan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), "aaa").unwrap();
        fs::write(dir.join("sub/b.txt"), "bb").unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("sub/loop")).unwrap();

        let mut plan = Vec::new();
        let mut skipped = 0;
        plan_local(&dir, "/r", &mut plan, &mut skipped, 0).unwrap();
        let remote: Vec<&str> = plan.iter().map(|i| i.remote.as_str()).collect();
        assert_eq!(remote, ["/r/a.txt", "/r/sub", "/r/sub/b.txt"]);
        assert_eq!(plan[0].size, 3);
        assert!(plan[1].is_dir);
        assert_eq!(skipped, 1, "the link to a folder is skipped");
        let _ = fs::remove_dir_all(&dir);
    }
}
