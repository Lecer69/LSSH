use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::tty::IsTty;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::commands::sshpass_available;
use crate::config::Config;
use crate::stats::{self, Stats};
use crate::ui;

/// Everything needed to open an interactive session.
pub struct Session {
    pub name: String,
    pub user: String,
    pub host: String,
    pub password: Option<String>,
    pub config: Config,
}

/// Entry point: use the PTY proxy + stats bar when attached to a TTY and the
/// bar is enabled, otherwise fall back to a plain `ssh` invocation.
pub fn run(session: &Session) -> Result<()> {
    let (cols, rows) = match crossterm::terminal::size() {
        Ok(size) if size.0 > 0 && size.1 > 0 => size,
        _ => return run_plain(session),
    };
    let bar_h = effective_bar_height(&session.config, rows);
    if bar_h == 0 || !is_tty() {
        return run_plain(session);
    }

    let control_dir = create_control_dir()?;
    let control_path = control_dir.join("sock");
    let _ = std::fs::remove_file(&control_path);

    let pty_rows = rows.saturating_sub(bar_h).max(1);
    let size = PtySize {
        rows: pty_rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(size).context("failed to open a pty")?;

    let command = build_command(session, &control_path);
    let mut child = pair
        .slave
        .spawn_command(command)
        .context("failed to launch ssh inside the pty")?;
    drop(pair.slave);

    let master = pair.master;
    let mut reader = match master.try_clone_reader() {
        Ok(reader) => reader,
        Err(e) => {
            kill_child(&mut child);
            return Err(e).context("failed to read pty");
        }
    };
    let mut writer = match master.take_writer() {
        Ok(writer) => writer,
        Err(e) => {
            kill_child(&mut child);
            return Err(e).context("failed to write pty");
        }
    };

    let theme = ui::Theme::from_name(&session.config.theme);
    let target = format!("{}@{}", session.user, session.host);

    // All terminal output goes through one lock so a whole bar repaint is
    // written atomically and can never interleave with remote output.
    let out = Arc::new(Mutex::new(io::stdout()));

    // Remote stats poller.
    let (stats_tx, stats_rx) = mpsc::channel::<Stats>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let stats_path = control_path.clone();
    let interval = session.config.update_interval_ms;
    let stats_thread = thread::spawn(move || {
        let mut collector = stats::Collector::new();
        loop {
            match stop_rx.recv_timeout(Duration::from_millis(interval)) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if let Ok(raw) = stats::collect(&target, &stats_path) {
                let snapshot = stats::parse(&raw, &mut collector);
                if stats_tx.send(snapshot).is_err() {
                    break;
                }
            }
        }
    });

    // Local stdin -> pty.
    let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if input_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // pty -> local stdout.
    let reader_out = Arc::clone(&out);
    let reader_thread = thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Ok(mut o) = reader_out.lock() else {
                        break;
                    };
                    if o.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = o.flush();
                }
            }
        }
    });

    if let Err(e) = enable_raw_mode() {
        kill_child(&mut child);
        return Err(e).context("failed to enable raw mode");
    }
    let mut guard = TermGuard::new(rows, bar_h, Arc::clone(&out));

    emit(
        &out,
        &format!(
            "\x1b[2J\x1b[H\x1b]2;lssh · {}\x07\x1b[1;{pty_rows}r",
            session.name
        ),
    );

    let mut cols = cols;
    let mut rows = rows;
    let mut bar_h = bar_h;
    let mut stats: Option<Stats> = None;
    let mut dirty = true;

    let result: Result<()> = (|| -> Result<()> {
        loop {
            match input_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(bytes) => {
                    writer.write_all(&bytes)?;
                    writer.flush()?;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            if child.try_wait()?.is_some() {
                break;
            }

            let mut got_stats = false;
            while let Ok(snapshot) = stats_rx.try_recv() {
                stats = Some(snapshot);
                got_stats = true;
            }
            if got_stats {
                dirty = true;
            }

            if let Ok((new_cols, new_rows)) = crossterm::terminal::size()
                && (new_cols != cols || new_rows != rows)
            {
                let old_rows = rows;
                let old_bar_h = bar_h;

                cols = new_cols;
                rows = new_rows;
                bar_h = effective_bar_height(&session.config, rows);
                let new_pty_rows = rows.saturating_sub(bar_h).max(1);

                let _ = master.resize(PtySize {
                    rows: new_pty_rows,
                    cols: new_cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                guard.update(rows, bar_h);

                // DECSTBM homes the cursor, so save it before changing the
                // region and restore it afterwards; otherwise the shell cursor
                // would jump to the top on every resize. Only touch the region
                // when the reserved height actually changed.
                emit(
                    &out,
                    &resize_sequence(old_rows, old_bar_h, rows, bar_h, new_pty_rows),
                );
                dirty = true;
            }

            if dirty && bar_h > 0 {
                let lines = ui::render_bar(
                    stats.as_ref(),
                    &session.name,
                    &session.user,
                    &session.host,
                    cols as usize,
                    bar_h as usize,
                    &session.config,
                    &theme,
                );
                emit(&out, &bar_string(rows, bar_h, &lines));
                dirty = false;
            }
        }
        Ok(())
    })();

    let _ = stop_tx.send(());
    let _ = stats_thread.join();
    let _ = reader_thread.join();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&control_path);
    let _ = std::fs::remove_dir(&control_dir);

    result
}

/// Height of the reserved bar, or 0 if it would leave no usable shell rows.
fn effective_bar_height(cfg: &Config, rows: u16) -> u16 {
    if rows < 4 || cfg.bar_height == 0 {
        0
    } else {
        cfg.bar_height.min(rows.saturating_sub(2))
    }
}

fn is_tty() -> bool {
    io::stdin().is_tty() && io::stdout().is_tty()
}

type PtyChild = Box<dyn portable_pty::Child + Send + Sync>;

/// Terminate and reap a child that we could not hand off to the event loop.
fn kill_child(child: &mut PtyChild) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Plain pass-through used when there is no TTY or the bar is disabled.
fn run_plain(session: &Session) -> Result<()> {
    let target = format!("{}@{}", session.user, session.host);
    let status = match session.password.as_deref() {
        Some(password) if sshpass_available() => std::process::Command::new("sshpass")
            .arg("-e")
            .arg("ssh")
            .arg(&target)
            .env("SSHPASS", password)
            .status()
            .context("failed to launch sshpass (is openssh installed?)")?,
        _ => std::process::Command::new("ssh")
            .arg(&target)
            .status()
            .context("failed to launch ssh (is openssh installed?)")?,
    };
    if !status.success() {
        bail!("ssh exited with {status}");
    }
    Ok(())
}

/// Build the interactive `ssh` (optionally via `sshpass`) command.
///
/// `ControlMaster=yes` turns this session into a multiplexing master so the
/// stats poller can reuse its authenticated connection.
fn build_command(session: &Session, control_path: &std::path::Path) -> CommandBuilder {
    let target = format!("{}@{}", session.user, session.host);
    let opts: Vec<String> = vec![
        "-o".into(),
        "ControlMaster=yes".into(),
        "-o".into(),
        format!("ControlPath={}", control_path.display()),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];

    let mut command = if session.password.is_some() && sshpass_available() {
        let mut c = CommandBuilder::new("sshpass");
        c.arg("-e");
        c.arg("ssh");
        c.args(opts);
        c.arg(&target);
        c
    } else {
        let mut command = CommandBuilder::new("ssh");
        command.args(opts);
        command.arg(&target);
        command
    };

    if let Some(password) = session.password.as_deref() {
        command.env("SSHPASS", password);
    }
    command
}

fn create_control_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("lssh-{}", std::process::id()));
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

/// Write a fully-formed escape sequence under the shared output lock.
fn emit(out: &Arc<Mutex<io::Stdout>>, text: &str) {
    if let Ok(mut o) = out.lock() {
        let _ = o.write_all(text.as_bytes());
        let _ = o.flush();
    }
}

fn clear_string(rows: u16, bar_h: u16) -> String {
    let mut out = String::new();
    for i in 0..bar_h {
        let row = rows.saturating_sub(bar_h) + 1 + i;
        out.push_str(&format!("\x1b[{};1H\x1b[2K", row));
    }
    out
}

/// Escape sequence emitted when the terminal is resized.
///
/// The shell cursor is saved/restored around the scroll-region change because
/// DECSTBM moves the cursor to the home position. When only the width changed
/// (same rows, same bar height) the region is left untouched entirely.
fn resize_sequence(
    old_rows: u16,
    old_bar_h: u16,
    new_rows: u16,
    new_bar_h: u16,
    new_pty_rows: u16,
) -> String {
    if new_bar_h == 0 {
        return format!("\x1b7\x1b[r{}\x1b8", clear_string(old_rows, old_bar_h));
    }
    let clear = clear_string(new_rows, new_bar_h);
    if old_rows != new_rows || old_bar_h != new_bar_h {
        format!("\x1b7\x1b[1;{new_pty_rows}r{clear}\x1b8")
    } else {
        format!("\x1b7{clear}\x1b8")
    }
}

/// Build the escape sequence that repaints the reserved bar lines.
fn bar_string(rows: u16, bar_h: u16, lines: &[String]) -> String {
    // Save cursor and disable autowrap so a full-width line can't scroll.
    let mut out = String::from("\x1b7\x1b[?7l");
    for (i, line) in lines.iter().enumerate() {
        if i >= bar_h as usize {
            break;
        }
        let row = rows.saturating_sub(bar_h) + 1 + i as u16;
        out.push_str(&format!("\x1b[{};1H\x1b[2K{}", row, line));
    }
    out.push_str("\x1b[?7h\x1b8");
    out
}

/// Restores the terminal on drop, even on panic or early return.
struct TermGuard {
    rows: u16,
    bar_h: u16,
    out: Arc<Mutex<io::Stdout>>,
}

impl TermGuard {
    fn new(rows: u16, bar_h: u16, out: Arc<Mutex<io::Stdout>>) -> Self {
        TermGuard { rows, bar_h, out }
    }

    fn update(&mut self, rows: u16, bar_h: u16) {
        self.rows = rows;
        self.bar_h = bar_h;
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        // Reset the scroll region, wipe the bar, park the cursor at the bottom.
        let text = format!(
            "\x1b[r{}\x1b[{};1H",
            clear_string(self.rows, self.bar_h),
            self.rows.max(1)
        );
        emit(&self.out, &text);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_height_disabled_for_tiny_terminals() {
        let cfg = Config::default();
        assert_eq!(effective_bar_height(&cfg, 3), 0);
        assert_eq!(effective_bar_height(&cfg, 4), 2);
        assert_eq!(effective_bar_height(&cfg, 24), 2);
    }

    #[test]
    fn bar_height_zero_when_configured_zero() {
        let cfg = Config {
            bar_height: 0,
            ..Config::default()
        };
        assert_eq!(effective_bar_height(&cfg, 40), 0);
    }

    #[test]
    fn bar_height_never_consumes_all_rows() {
        let cfg = Config {
            bar_height: 6,
            ..Config::default()
        };
        assert_eq!(effective_bar_height(&cfg, 5), 3);
    }

    #[test]
    fn bar_string_positions_each_line() {
        let lines = vec!["one".to_string(), "two".to_string()];
        let s = bar_string(10, 2, &lines);
        assert!(s.contains("\x1b[9;1H\x1b[2Kone"));
        assert!(s.contains("\x1b[10;1H\x1b[2Ktwo"));
        assert!(s.starts_with("\x1b7\x1b[?7l"));
        assert!(s.ends_with("\x1b[?7h\x1b8"));
    }

    #[test]
    fn clear_string_clears_bar_rows() {
        let s = clear_string(10, 2);
        assert!(s.contains("\x1b[9;1H\x1b[2K"));
        assert!(s.contains("\x1b[10;1H\x1b[2K"));
    }

    #[test]
    fn resize_resets_region_only_when_height_changes() {
        // Rows changed: region is rebuilt and the cursor is saved/restored.
        let s = resize_sequence(24, 2, 30, 2, 28);
        assert!(s.starts_with("\x1b7"));
        assert!(s.ends_with("\x1b8"));
        assert!(s.contains("\x1b[1;28r"));

        // Only the width changed (same rows, same bar): no region reset, so
        // the cursor can never be yanked home on a column-only resize.
        let s = resize_sequence(30, 2, 30, 2, 28);
        assert!(s.starts_with("\x1b7"));
        assert!(s.ends_with("\x1b8"));
        assert!(!s.contains("\x1b[1;"));
        assert!(!s.contains("\x1b[r"));
    }

    #[test]
    fn resize_disables_region_when_bar_hidden() {
        // Terminal shrank below the bar threshold: drop the region entirely.
        let s = resize_sequence(24, 2, 3, 0, 3);
        assert!(s.contains("\x1b[r"));
        assert!(!s.contains("\x1b[1;"));
    }
}
