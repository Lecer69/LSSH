//! `lssh cp`: copy files over SFTP, with a progress bar.
//!
//! The transfer uses lssh's own SFTP client (the one behind the file
//! panel) over a fresh ssh connection, so it can show exactly how many
//! bytes have gone through.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use colored::Colorize;
use unicode_width::UnicodeWidthStr;

use crate::askpass::Askpass;
use crate::files::{self, Item, human_size};
use crate::out;
use crate::proxy;
use crate::sftp::{self, Sftp, StatusError};
use crate::store::{Server, Store};

/// Which way the files go.
enum Direction<'a> {
    Download { remote: &'a str, local: &'a str },
    Upload { local: &'a str, remote: &'a str },
}

pub fn cp(from: &str, to: &str, recursive: bool, use_proxy: bool) -> Result<()> {
    let store = Store::open()?;
    let (server, direction) = match (on_server(&store, from), on_server(&store, to)) {
        (Some((server, remote)), None) => (server, Direction::Download { remote, local: to }),
        (None, Some((server, remote))) => (
            server,
            Direction::Upload {
                local: from,
                remote,
            },
        ),
        (Some(_), Some(_)) => {
            bail!("copying between two servers isn't supported\ncopy to this machine first")
        }
        (None, None) => bail!(
            "neither '{from}' nor '{to}' is on a saved server\n\
             write remote paths as <server>:<path>, e.g. web:/etc/hosts"
        ),
    };
    // Check the local side before logging in.
    if let Direction::Upload { local, .. } = direction {
        let meta = fs::metadata(local).with_context(|| format!("cannot read {local}"))?;
        if meta.is_dir() && !recursive {
            bail!("{local} is a folder\nto copy it with everything in it: lssh cp -r {from} {to}");
        }
    }

    let proxy_options = proxy::ssh_options(proxy::resolve(use_proxy)?.as_ref())?;
    let askpass = Askpass::new(server, true)?;
    let mut ssh = Command::new("ssh");
    askpass.apply(&mut ssh);
    // Keepalives make a stalled connection fail instead of hanging.
    ssh.args([
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
    ])
        .args(server.ssh_options())
        .args(&proxy_options);
    let mut sftp = Sftp::start(ssh, &server.target())
        .with_context(|| format!("could not open sftp on {}", server.address()))?;

    let (plan, skipped) = match direction {
        Direction::Download { remote, local } => {
            plan_download(&mut sftp, remote, local, recursive)?
        }
        Direction::Upload { local, remote } => plan_upload(&mut sftp, local, remote)?,
    };
    let upload = matches!(direction, Direction::Upload { .. });
    let copied = transfer(&mut sftp, &plan, upload)?;

    let mut summary = match copied.files {
        1 => human_size(copied.bytes),
        n => format!("{n} files, {}", human_size(copied.bytes)),
    };
    if skipped > 0 {
        summary.push_str(&format!(", skipped {skipped}"));
    }
    let rate = copied.bytes as f64 / copied.elapsed.as_secs_f64().max(0.001);
    if !out::raw() {
        summary.push_str(&format!(
            " in {} at {}/s",
            human_duration(copied.elapsed),
            human_size(rate as u64)
        ));
    }
    out::success(format_args!(
        "copied {from} → {to} {}",
        format!("({summary})").dimmed()
    ));
    Ok(())
}

/// Split `<server>:<path>` when `<server>` is a saved server. An empty
/// path is the remote home folder.
fn on_server<'a>(store: &'a Store, spec: &'a str) -> Option<(&'a Server, &'a str)> {
    let (name, path) = spec.split_once(':')?;
    let path = if path.is_empty() { "." } else { path };
    Some((store.get(name).ok()?, path))
}

/// Where a copied `name` lands when the destination is `dest`: inside it
/// when it is an existing folder (or written as one, with a trailing `/`),
/// otherwise at `dest` itself. Like cp and scp.
fn local_target(dest: &str, name: &str) -> PathBuf {
    let path = Path::new(dest);
    if path.is_dir() || dest.ends_with('/') {
        path.join(name)
    } else {
        path.to_path_buf()
    }
}

fn plan_download(
    sftp: &mut Sftp,
    remote: &str,
    local: &str,
    recursive: bool,
) -> Result<(Vec<Item>, u64)> {
    let remote = sftp
        .realpath(remote)
        .map_err(|e| e.context(format!("cannot find {remote}")))?;
    let attrs = sftp
        .stat(&remote)
        .map_err(|e| e.context(format!("cannot find {remote}")))?;
    let name = sftp::file_name(&remote);
    let name = if name.is_empty() { "root" } else { name };
    let target = local_target(local, name);
    let mut plan = Vec::new();
    let mut skipped = 0;
    if attrs.is_dir() {
        if !recursive {
            bail!("{remote} is a folder\nto copy it with everything in it, add -r");
        }
        plan.push(Item::dir(target.clone(), remote.clone()));
        files::plan_remote(sftp, &remote, &target, &mut plan, &mut skipped, 0, &|| {
            false
        })?;
    } else if attrs.is_file() {
        plan.push(Item::file(
            target,
            remote,
            attrs.size.unwrap_or(0),
            attrs.permissions.unwrap_or(0o644),
        ));
    } else {
        bail!("{remote} is not a file or folder");
    }
    Ok((plan, skipped))
}

fn plan_upload(sftp: &mut Sftp, local: &str, remote: &str) -> Result<(Vec<Item>, u64)> {
    let source = PathBuf::from(local);
    let meta = fs::metadata(&source).with_context(|| format!("cannot read {local}"))?;
    let name = fs::canonicalize(&source)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .with_context(|| format!("{local} has no file name"))?;
    // Into the folder when it exists (or is written as one), else as it.
    let into_dir = remote.ends_with('/') || sftp.stat(remote).is_ok_and(|a| a.is_dir());
    let target = if into_dir {
        sftp::join(remote, &name)
    } else {
        remote.to_string()
    };
    let mut plan = Vec::new();
    let mut skipped = 0;
    if meta.is_dir() {
        plan.push(Item::dir(source.clone(), target.clone()));
        files::plan_local(&source, &target, &mut plan, &mut skipped, 0)?;
    } else if meta.is_file() {
        plan.push(Item::file(
            source,
            target,
            meta.len(),
            files::mode_of(&meta),
        ));
    } else {
        bail!("{local} is not a file or folder");
    }
    Ok((plan, skipped))
}

/// What a transfer moved.
struct Copied {
    files: u64,
    bytes: u64,
    elapsed: Duration,
}

fn transfer(sftp: &mut Sftp, plan: &[Item], upload: bool) -> Result<Copied> {
    let total_files = plan.iter().filter(|i| !i.is_dir).count() as u64;
    let total_bytes = plan.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();
    let mut bar = Bar::new(total_bytes, total_files);
    let mut base = 0u64;
    let mut files = 0u64;

    for item in plan {
        if item.is_dir {
            if upload {
                if let Err(e) = sftp.mkdir(&item.remote)
                    && !sftp.stat(&item.remote).is_ok_and(|a| a.is_dir())
                {
                    bar.clear();
                    return Err(e.context(format!("cannot create {}", item.remote)));
                }
            } else {
                fs::create_dir_all(&item.local)
                    .with_context(|| format!("cannot create {}", item.local.display()))
                    .inspect_err(|_| bar.clear())?;
            }
            continue;
        }

        let name = sftp::file_name(&item.remote).to_string();
        bar.start_file(&name, files);
        let result = if upload {
            copy_up(sftp, item, &mut |done| {
                bar.update(base + done);
                true
            })
        } else {
            copy_down(sftp, item, &mut |done| {
                bar.update(base + done);
                true
            })
        };
        if let Err(e) = result {
            bar.clear();
            let verb = if upload { "uploading" } else { "downloading" };
            return Err(e.context(format!("{verb} {name}")));
        }
        base += item.size;
        files += 1;
        bar.update(base);
    }
    bar.clear();
    Ok(Copied {
        files,
        bytes: base,
        elapsed: bar.started.elapsed(),
    })
}

fn copy_up(sftp: &mut Sftp, item: &Item, progress: &mut dyn FnMut(u64) -> bool) -> Result<()> {
    let mut file =
        File::open(&item.local).with_context(|| format!("cannot open {}", item.local.display()))?;
    let copied = sftp.upload(&mut file, item.size, &item.remote, item.mode, progress);
    if let Err(e) = &copied
        && e.downcast_ref::<StatusError>().is_some()
    {
        // Don't leave a half-written file on the server.
        let _ = sftp.remove(&item.remote);
    }
    copied
}

fn copy_down(sftp: &mut Sftp, item: &Item, progress: &mut dyn FnMut(u64) -> bool) -> Result<()> {
    let file = File::create(&item.local)
        .with_context(|| format!("cannot create {}", item.local.display()))?;
    if let Err(e) = sftp.download(&item.remote, item.size, &file, progress) {
        drop(file);
        let _ = fs::remove_file(&item.local);
        return Err(e);
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
    Ok(())
}

/// How often the bar is redrawn.
const REDRAW: Duration = Duration::from_millis(80);
/// The speed is averaged over this much recent time.
const RATE_WINDOW: Duration = Duration::from_secs(3);

/// A one-line progress bar on stderr, drawn only on a terminal and
/// never in raw mode:
///
/// `  42% ━━━━━━━━━━━━╸━━━━━━━━━━━━━  12M / 28M  3.4M/s  0:05 left  file.iso`
struct Bar {
    enabled: bool,
    total_bytes: u64,
    total_files: u64,
    file: String,
    file_index: u64,
    started: Instant,
    last_draw: Option<Instant>,
    samples: VecDeque<(Instant, u64)>,
    drawn: bool,
}

impl Bar {
    fn new(total_bytes: u64, total_files: u64) -> Bar {
        Bar {
            enabled: !out::raw() && std::io::stderr().is_terminal(),
            total_bytes,
            total_files,
            file: String::new(),
            file_index: 0,
            started: Instant::now(),
            last_draw: None,
            samples: VecDeque::new(),
            drawn: false,
        }
    }

    fn start_file(&mut self, name: &str, index: u64) {
        self.file = name.to_string();
        self.file_index = index;
        self.last_draw = None;
    }

    fn update(&mut self, bytes: u64) {
        let now = Instant::now();
        self.samples.push_back((now, bytes));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > RATE_WINDOW)
            && self.samples.len() > 2
        {
            self.samples.pop_front();
        }
        if !self.enabled || self.last_draw.is_some_and(|t| t.elapsed() < REDRAW) {
            return;
        }
        self.last_draw = Some(now);
        let width = crossterm::terminal::size().map_or(80, |(w, _)| w as usize);
        let line = self.render(bytes, width);
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K{line}");
        let _ = err.flush();
        self.drawn = true;
    }

    /// Bytes per second over the recent samples.
    fn rate(&self) -> Option<f64> {
        let (t0, b0) = self.samples.front()?;
        let (t1, b1) = self.samples.back()?;
        let secs = t1.duration_since(*t0).as_secs_f64();
        (secs >= 0.2).then(|| b1.saturating_sub(*b0) as f64 / secs)
    }

    fn render(&self, bytes: u64, width: usize) -> String {
        let fraction = if self.total_bytes == 0 {
            1.0
        } else {
            (bytes as f64 / self.total_bytes as f64).min(1.0)
        };
        let percent = format!("{:>3}%", (fraction * 100.0).floor() as u64);
        let amount = format!("{} / {}", human_size(bytes), human_size(self.total_bytes));
        let rate = self.rate();
        let speed = match rate {
            Some(rate) => format!("{}/s", human_size(rate as u64)),
            None => "…".to_string(),
        };
        let left = match rate {
            Some(rate) if rate > 0.0 => {
                let secs = self.total_bytes.saturating_sub(bytes) as f64 / rate;
                format!(
                    "{} left",
                    human_duration(Duration::from_secs_f64(secs.min(1e7)))
                )
            }
            _ => String::new(),
        };
        let mut name = self.file.clone();
        if width < 2 + percent.len() + 2 + amount.len() + 2 + speed.len() + 4 {
            // Too narrow for the rest.
            return format!(
                "  {}  {}",
                percent.bold(),
                truncate(&amount, width.saturating_sub(10))
            );
        }
        if self.total_files > 1 {
            name = format!("[{}/{}] {name}", self.file_index + 1, self.total_files);
        }

        // Fixed parts first; the bar takes what is left (10 to 40 cells),
        // and the file name gets the rest.
        let fixed = 2
            + percent.len()
            + 2
            + amount.len()
            + 2
            + speed.len()
            + if left.is_empty() { 0 } else { 2 + left.len() };
        let bar_width = width.saturating_sub(fixed + 4).clamp(0, 40);
        let mut line = format!("  {} ", percent.bold());
        if bar_width >= 10 {
            line.push_str(&draw_bar(fraction, bar_width));
            line.push(' ');
        }
        line.push_str(&format!(" {}  {}", amount, speed.cyan()));
        if !left.is_empty() {
            line.push_str(&format!("  {}", left.dimmed()));
        }
        let used = fixed + if bar_width >= 10 { bar_width + 1 } else { 0 };
        let room = width.saturating_sub(used + 3);
        if room >= 6 {
            line.push_str(&format!("  {}", truncate(&name, room).dimmed()));
        }
        line
    }

    /// Wipe the bar so the next message starts on a clean line.
    fn clear(&mut self) {
        if self.drawn {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
            self.drawn = false;
        }
    }
}

/// `━━━━━━╸━━━━━`: the done part in green, the rest dimmed.
fn draw_bar(fraction: f64, width: usize) -> String {
    let halves = (fraction * (width * 2) as f64).round() as usize;
    let full = halves / 2;
    let half = halves % 2 == 1;
    let mut done = "━".repeat(full);
    let mut rest_len = width - full;
    if half {
        done.push('╸');
        rest_len -= 1;
    }
    format!("{}{}", done.green(), "━".repeat(rest_len).bright_black())
}

/// Keep the end of `text` (the file name matters more than its folder).
fn truncate(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut width = 1;
    for c in text.chars().rev() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > max {
            break;
        }
        width += w;
        tail.push(c);
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

/// `0:05`, `3:20`, `1:02:03`.
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 1 && d.as_millis() > 0 {
        return format!("{}ms", d.as_millis());
    }
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_need_a_saved_server() {
        let path = std::env::temp_dir().join(format!("lssh-test-cp-{}.json", std::process::id()));
        let mut store = Store::open_at(path).unwrap();
        store
            .add(Server::new("web", "root@10.0.0.1").unwrap())
            .unwrap();
        let (server, rest) = on_server(&store, "web:/etc/hosts").unwrap();
        assert_eq!((server.name.as_str(), rest), ("web", "/etc/hosts"));
        assert_eq!(on_server(&store, "web:").unwrap().1, ".");
        assert!(on_server(&store, "db:/etc/hosts").is_none());
        assert!(on_server(&store, "./local").is_none());
    }

    #[test]
    fn bar_fills_in_half_cells() {
        colored::control::set_override(false);
        assert_eq!(draw_bar(0.0, 4), "━━━━");
        assert_eq!(draw_bar(0.5, 4).chars().count(), 4);
        assert_eq!(draw_bar(0.375, 4), "━╸━━");
        assert_eq!(draw_bar(1.0, 4), "━━━━");
    }

    #[test]
    fn bar_line_fits_the_terminal() {
        colored::control::set_override(false);
        let mut bar = Bar::new(100 * 1024 * 1024, 3);
        bar.start_file("a-rather-long-file-name.tar.gz", 1);
        for width in [12, 20, 40, 80, 200] {
            let line = bar.render(42 * 1024 * 1024, width);
            assert!(
                line.width() <= width,
                "{width}: {line:?} is {}",
                line.width()
            );
            assert!(line.contains("42%"), "{width}: {line}");
        }
        assert!(bar.render(0, 200).contains("[2/3]"));
    }

    #[test]
    fn helpers() {
        assert_eq!(truncate("hello.txt", 20), "hello.txt");
        assert_eq!(truncate("abcdefghij", 5), "…ghij");
        assert_eq!(human_duration(Duration::from_secs(5)), "0:05");
        assert_eq!(human_duration(Duration::from_secs(200)), "3:20");
        assert_eq!(human_duration(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(human_duration(Duration::from_millis(250)), "250ms");
    }

    #[test]
    fn local_targets_follow_cp() {
        let dir = std::env::temp_dir();
        let dir_str = dir.to_string_lossy().to_string();
        assert_eq!(local_target(&dir_str, "x"), dir.join("x"));
        assert_eq!(local_target("new-name", "x"), PathBuf::from("new-name"));
        assert_eq!(local_target("newdir/", "x"), PathBuf::from("newdir/x"));
    }
}
