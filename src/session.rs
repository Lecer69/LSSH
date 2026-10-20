use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::tty::IsTty;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::config::Config;
use crate::hostkey;
use crate::stats::{self, Stats};
use crate::ui;
use crate::vt;

/// Everything needed to open an interactive session.
pub struct Session {
    pub name: String,
    pub user: String,
    pub host: String,
    /// Password replayed through sshpass. `None` lets ssh prompt by itself.
    pub password: Option<String>,
    pub config: Config,
}

/// Check that `sshpass` is installed. ssh only reads passwords from the
/// TTY, so sshpass is needed to feed it one non-interactively.
pub fn sshpass_available() -> bool {
    std::process::Command::new("sshpass")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Entry point: use the PTY proxy + stats bar when attached to a TTY and the
/// bar is enabled, otherwise fall back to a plain `ssh` invocation.
pub fn run(session: &Session) -> Result<()> {
    let use_sshpass = session.password.is_some();
    let target = format!("{}@{}", session.user, session.host);
    if use_sshpass {
        // sshpass bails out on ssh's "continue connecting?" prompt, so
        // settle the host key first while the terminal is still normal.
        hostkey::ensure_known(&target)?;
    }

    let (cols, rows) = match crossterm::terminal::size() {
        Ok(size) if size.0 > 0 && size.1 > 0 => size,
        _ => return run_plain(session, use_sshpass),
    };
    let layout = Layout::new(&session.config, cols, rows);
    if layout.bar_h == 0 || !is_tty() {
        return run_plain(session, use_sshpass);
    }

    let control_dir = create_control_dir()?;
    let control_path = control_dir.join("sock");
    let _ = std::fs::remove_file(&control_path);

    let mut pty_size = (layout.pty_rows(), cols);
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: pty_size.0,
            cols: pty_size.1,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open a pty")?;

    let command = build_command(session, &control_path, use_sshpass);
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
    let render = |layout: &Layout, stats: Option<&Stats>| {
        ui::render_bar(
            stats,
            &session.name,
            &session.user,
            &session.host,
            layout.cols as usize,
            layout.bar_h as usize,
            &session.config,
            &theme,
        )
    };

    // Remote stats poller.
    let (stats_tx, stats_rx) = mpsc::channel::<Stats>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let stats_path = control_path.clone();
    let stats_target = target.clone();
    let interval = session.config.update_interval_ms;
    let stats_thread = thread::spawn(move || {
        let mut collector = stats::Collector::new();
        loop {
            match stop_rx.recv_timeout(Duration::from_millis(interval)) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            // Until the session has authenticated there is no master socket,
            // and ssh would open a fresh (failing) connection every tick.
            if !stats_path.exists() {
                continue;
            }
            if let Ok(raw) = stats::collect(&stats_target, &stats_path) {
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

    if let Err(e) = enable_raw_mode() {
        kill_child(&mut child);
        return Err(e).context("failed to enable raw mode");
    }

    // All terminal output goes through one lock so a whole bar repaint is
    // written atomically and can never interleave with remote output.
    let screen = Arc::new(Mutex::new(Screen::new(layout, render(&layout, None))));
    let guard = TermGuard(Arc::clone(&screen));

    // Set up the screen before any remote output is shown, so an early
    // prompt (host key, password) can't be wiped by the initial clear.
    if let Ok(mut s) = screen.lock() {
        let init = format!(
            "\x1b[2J\x1b[H\x1b]2;lssh · {}\x07\x1b[1;{}r",
            session.name,
            layout.pty_rows()
        );
        let _ = s.out.write_all(init.as_bytes());
        let _ = s.paint_bar_if_ready();
        let _ = s.out.flush();
    }

    // pty -> local stdout.
    let reader_screen = Arc::clone(&screen);
    let reader_thread = thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Ok(mut s) = reader_screen.lock() else {
                        break;
                    };
                    if s.write_remote(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut layout = layout;
    let mut stats: Option<Stats> = None;
    let mut pending_fixup = false;
    let mut fixup_waits = 0u32;
    // A cursor report that arrived after we stopped waiting for it must not
    // be forwarded to the remote as keyboard input.
    let mut late_report: Option<Instant> = None;
    let mut exit_status = None;

    let result: Result<()> = (|| -> Result<()> {
        loop {
            match input_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(mut bytes) => {
                    if let Some(deadline) = late_report
                        && (Instant::now() > deadline
                            || vt::take_cursor_report(&mut bytes).is_some())
                    {
                        late_report = None;
                    }
                    writer.write_all(&bytes)?;
                    writer.flush()?;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            if let Some(status) = child.try_wait()? {
                exit_status = Some(status);
                break;
            }

            let mut got_stats = false;
            while let Ok(snapshot) = stats_rx.try_recv() {
                stats = Some(snapshot);
                got_stats = true;
            }

            if let Ok((new_cols, new_rows)) = crossterm::terminal::size()
                && (new_cols != layout.cols || new_rows != layout.rows)
                && new_cols > 0
                && new_rows > 0
            {
                layout = Layout::new(&session.config, new_cols, new_rows);
                pending_fixup = true;
            }

            let Ok(mut s) = screen.lock() else {
                break;
            };
            if std::mem::take(&mut s.needs_fixup) {
                pending_fixup = true;
            }

            if pending_fixup {
                // Never inject our sequences into the middle of the remote's;
                // give up waiting after ~half a second though.
                if !s.filter.at_ground() && fixup_waits < 10 {
                    fixup_waits += 1;
                    continue;
                }
                fixup_waits = 0;
                pending_fixup = false;

                s.layout = layout;
                s.filter.set_pty_rows(layout.pty_rows());
                s.bar = render(&layout, stats.as_ref());
                let (leftover, answered) = s.fixup(&input_rx)?;
                drop(s);
                if !answered {
                    late_report = Some(Instant::now() + Duration::from_secs(2));
                }
                if !leftover.is_empty() {
                    writer.write_all(&leftover)?;
                    writer.flush()?;
                }

                // Resize the remote only after the region is in place, so
                // its SIGWINCH redraw lands in the right area.
                let new_size = (layout.pty_rows(), layout.cols);
                if new_size != pty_size {
                    pty_size = new_size;
                    let _ = master.resize(PtySize {
                        rows: new_size.0,
                        cols: new_size.1,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            } else {
                if got_stats {
                    let applied = s.layout;
                    s.bar = render(&applied, stats.as_ref());
                    s.bar_dirty = true;
                }
                s.paint_bar_if_ready()?;
                s.out.flush()?;
            }
        }
        Ok(())
    })();

    let _ = stop_tx.send(());
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader_thread.join();
    let _ = stats_thread.join();
    let _ = std::fs::remove_file(&control_path);
    let _ = std::fs::remove_dir(&control_dir);
    drop(guard);

    result?;
    if use_sshpass
        && let Some(status) = exit_status
        && let Some(reason) = sshpass_failure(status.exit_code(), session)
    {
        bail!("{reason}");
    }
    Ok(())
}

/// Explain sshpass' own exit codes, which otherwise just look like the
/// session closing immediately.
fn sshpass_failure(code: u32, session: &Session) -> Option<String> {
    match code {
        5 => Some(format!(
            "the saved password was rejected by the server\n\
             update it with: lssh passwd {}",
            session.name
        )),
        6 => Some("the server's host key is unknown and was not confirmed".to_string()),
        7 => Some(format!(
            "the server's host key has CHANGED\n\
             verify it, then remove the old entry with: ssh-keygen -R {}",
            session.host
        )),
        _ => None,
    }
}

/// Screen geometry: shell rows on top, then `gap` blank rows, then the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    cols: u16,
    rows: u16,
    bar_h: u16,
    gap: u16,
}

impl Layout {
    fn new(cfg: &Config, cols: u16, rows: u16) -> Self {
        let bar_h = effective_bar_height(cfg, rows);
        let gap = if bar_h == 0 {
            0
        } else {
            cfg.bar_spacing.min(rows.saturating_sub(bar_h + 2))
        };
        Layout {
            cols,
            rows,
            bar_h,
            gap,
        }
    }

    /// Rows handed to the remote shell.
    fn pty_rows(&self) -> u16 {
        self.rows.saturating_sub(self.bar_h + self.gap).max(1)
    }

    /// First (1-based) row of the bar itself.
    fn bar_top(&self) -> u16 {
        self.rows.saturating_sub(self.bar_h) + 1
    }
}

/// Everything that writes to the terminal, behind one lock.
struct Screen {
    out: io::Stdout,
    filter: vt::Filter,
    /// The layout currently in effect on the terminal.
    layout: Layout,
    /// Rendered bar lines for `layout`.
    bar: Vec<String>,
    bar_dirty: bool,
    /// Set by the reader when the remote left the alt screen.
    needs_fixup: bool,
}

impl Screen {
    fn new(layout: Layout, bar: Vec<String>) -> Self {
        Screen {
            out: io::stdout(),
            filter: vt::Filter::new(layout.pty_rows()),
            layout,
            bar,
            bar_dirty: true,
            needs_fixup: false,
        }
    }

    /// Forward remote output, repainting the bar right away if it was erased.
    fn write_remote(&mut self, data: &[u8]) -> io::Result<()> {
        let mut filtered = Vec::with_capacity(data.len() + 16);
        let ev = self.filter.feed(data, &mut filtered);
        self.out.write_all(&filtered)?;
        self.bar_dirty |= ev.repaint;
        self.needs_fixup |= ev.fixup;
        self.paint_bar_if_ready()?;
        self.out.flush()
    }

    /// Redraw the bar if needed and the remote stream is between sequences.
    /// The caller flushes.
    fn paint_bar_if_ready(&mut self) -> io::Result<()> {
        if self.bar_dirty && self.filter.at_ground() && self.layout.bar_h > 0 {
            let text = bar_string(&self.layout, &self.bar, self.filter.autowrap());
            self.out.write_all(text.as_bytes())?;
            self.bar_dirty = false;
        }
        Ok(())
    }

    /// Re-establish the layout after a resize (or alt-screen exit).
    ///
    /// Terminals reflow, scroll or pad the screen in different ways when
    /// resized, which leaves old bar lines in the shell area and can reset
    /// the scroll region. Ask the terminal where the cursor ended up, then
    /// wipe everything below it (old bar remnants always end up below the
    /// cursor), set the region and redraw the bar.
    ///
    /// Returns keyboard input received while waiting, and whether the
    /// terminal answered the cursor query.
    fn fixup(&mut self, input_rx: &Receiver<Vec<u8>>) -> io::Result<(Vec<u8>, bool)> {
        self.out.write_all(b"\x1b[6n")?;
        self.out.flush()?;

        let deadline = Instant::now() + Duration::from_millis(300);
        let mut pending = Vec::new();
        let mut cursor = None;
        while cursor.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match input_rx.recv_timeout(left) {
                Ok(bytes) => {
                    pending.extend_from_slice(&bytes);
                    cursor = vt::take_cursor_report(&mut pending);
                }
                Err(_) => break,
            }
        }

        let autowrap = self.filter.autowrap();
        let text = match cursor {
            Some((row, col)) => fixup_sequence(&self.layout, row, col, &self.bar, autowrap),
            None => fallback_sequence(&self.layout, &self.bar, autowrap),
        };
        self.out.write_all(text.as_bytes())?;
        self.out.flush()?;
        self.bar_dirty = false;
        Ok((pending, cursor.is_some()))
    }
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
fn run_plain(session: &Session, use_sshpass: bool) -> Result<()> {
    let target = format!("{}@{}", session.user, session.host);
    let status = match session.password.as_deref() {
        Some(password) if use_sshpass => std::process::Command::new("sshpass")
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
    if use_sshpass
        && let Some(reason) = status
            .code()
            .and_then(|c| u32::try_from(c).ok())
            .and_then(|code| sshpass_failure(code, session))
    {
        bail!("{reason}");
    }
    if !status.success() {
        bail!("ssh exited with {status}");
    }
    Ok(())
}

/// Build the interactive `ssh` (optionally via `sshpass`) command.
///
/// `ControlMaster=yes` turns this session into a multiplexing master so the
/// stats poller can reuse its authenticated connection.
fn build_command(
    session: &Session,
    control_path: &std::path::Path,
    use_sshpass: bool,
) -> CommandBuilder {
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

    let mut command = if use_sshpass {
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

/// The rows that belong to the bar and the gap above it, drawn without
/// moving the saved cursor. Autowrap is off while drawing so a full-width
/// line can't scroll the screen.
fn bar_body(layout: &Layout, lines: &[String], autowrap: bool) -> String {
    let mut out = String::from("\x1b[?7l");
    let first_gap = layout.pty_rows() + 1;
    for row in first_gap..layout.bar_top() {
        out.push_str(&format!("\x1b[{row};1H\x1b[2K"));
    }
    for (i, line) in lines.iter().take(layout.bar_h as usize).enumerate() {
        let row = layout.bar_top() + i as u16;
        out.push_str(&format!("\x1b[{row};1H\x1b[2K{line}"));
    }
    if autowrap {
        out.push_str("\x1b[?7h");
    }
    out
}

/// Build the escape sequence that repaints the reserved bar lines.
///
/// The cursor (and with it the remote's colours and origin mode) is saved
/// and restored around the paint, and attributes are reset so the remote's
/// current background colour can't bleed into the bar.
fn bar_string(layout: &Layout, lines: &[String], autowrap: bool) -> String {
    format!(
        "\x1b7\x1b[0m\x1b[?6l{}\x1b8",
        bar_body(layout, lines, autowrap)
    )
}

/// Layout repair when the cursor position is known (see [`Screen::fixup`]).
fn fixup_sequence(layout: &Layout, row: u16, col: u16, bar: &[String], autowrap: bool) -> String {
    let pty_rows = layout.pty_rows();
    let mut row = row.clamp(1, layout.rows.max(1));
    // Full-screen region first (this homes the cursor, hence the CUPs).
    let mut out = String::from("\x1b7\x1b[0m\x1b[?6l\x1b[r");
    if row > pty_rows {
        // The cursor ended up where the bar now lives: scroll the shell
        // content up (into scrollback) until it fits.
        out.push_str(&format!("\x1b[{};1H", layout.rows));
        out.push_str(&"\n".repeat((row - pty_rows) as usize));
        row = pty_rows;
    }
    if row < layout.rows {
        out.push_str(&format!("\x1b[{};1H\x1b[J", row + 1));
    }
    out.push_str(&format!("\x1b[1;{pty_rows}r"));
    out.push_str(&bar_body(layout, bar, autowrap));
    out.push_str(&format!("\x1b8\x1b[{row};{col}H"));
    out
}

/// Layout repair when the terminal did not report the cursor position:
/// just clear the reserved rows, set the region and redraw.
fn fallback_sequence(layout: &Layout, bar: &[String], autowrap: bool) -> String {
    let pty_rows = layout.pty_rows();
    let mut out = String::from("\x1b7\x1b[0m\x1b[?6l\x1b[r");
    if pty_rows < layout.rows {
        out.push_str(&format!("\x1b[{};1H\x1b[J", pty_rows + 1));
    }
    out.push_str(&format!("\x1b[1;{pty_rows}r"));
    out.push_str(&bar_body(layout, bar, autowrap));
    out.push_str("\x1b8");
    out
}

/// Sequence that hands the terminal back: leave a stranded alt screen,
/// restore modes the remote may have changed, drop the scroll region and
/// wipe the bar, keeping the cursor where the session left it.
fn restore_string(layout: &Layout, alt_screen: bool) -> String {
    let mut out = String::new();
    if alt_screen {
        out.push_str("\x1b[?1049l");
    }
    out.push_str("\x1b[0m\x1b[?7h\x1b[?25h\x1b7\x1b[r");
    let pty_rows = layout.pty_rows();
    if pty_rows < layout.rows {
        out.push_str(&format!("\x1b[{};1H\x1b[J", pty_rows + 1));
    }
    out.push_str("\x1b8");
    out
}

/// Restores the terminal on drop, even on panic or early return.
struct TermGuard(Arc<Mutex<Screen>>);

impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut s = match self.0.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        let text = restore_string(&s.layout, s.filter.alt_screen());
        let _ = s.out.write_all(text.as_bytes());
        let _ = s.out.flush();
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

    fn layout(rows: u16, bar_h: u16, gap: u16) -> Layout {
        Layout {
            cols: 80,
            rows,
            bar_h,
            gap,
        }
    }

    #[test]
    fn layout_reserves_spacing_above_bar() {
        let cfg = Config::default();
        let l = Layout::new(&cfg, 80, 24);
        assert_eq!((l.bar_h, l.gap), (2, 1));
        assert_eq!(l.pty_rows(), 21);
        assert_eq!(l.bar_top(), 23);

        let cfg = Config {
            bar_spacing: 0,
            ..Config::default()
        };
        assert_eq!(Layout::new(&cfg, 80, 24).pty_rows(), 22);
    }

    #[test]
    fn layout_drops_spacing_before_shell_rows() {
        let cfg = Config::default();
        // 4 rows: 2 for the bar, 2 left for the shell, no room for a gap.
        let l = Layout::new(&cfg, 80, 4);
        assert_eq!((l.bar_h, l.gap, l.pty_rows()), (2, 0, 2));
        let l = Layout::new(&cfg, 80, 3);
        assert_eq!((l.bar_h, l.gap, l.pty_rows()), (0, 0, 3));
    }

    #[test]
    fn bar_string_positions_each_line() {
        let lines = vec!["one".to_string(), "two".to_string()];
        let s = bar_string(&layout(10, 2, 1), &lines, true);
        assert!(s.contains("\x1b[8;1H\x1b[2K\x1b[9;1H"), "gap row cleared");
        assert!(s.contains("\x1b[9;1H\x1b[2Kone"));
        assert!(s.contains("\x1b[10;1H\x1b[2Ktwo"));
        assert!(s.starts_with("\x1b7\x1b[0m"));
        assert!(s.ends_with("\x1b[?7h\x1b8"));
    }

    #[test]
    fn bar_string_keeps_remote_autowrap_off() {
        let s = bar_string(&layout(10, 2, 0), &[], false);
        assert!(!s.contains("\x1b[?7h"));
    }

    #[test]
    fn fixup_clears_below_cursor_and_restores_it() {
        let l = layout(24, 2, 1);
        let s = fixup_sequence(&l, 5, 7, &[], true);
        assert!(s.contains("\x1b[r\x1b[6;1H\x1b[J\x1b[1;21r"));
        assert!(s.ends_with("\x1b8\x1b[5;7H"));
        assert!(!s.contains('\n'));
    }

    #[test]
    fn fixup_scrolls_cursor_out_of_bar_area() {
        let l = layout(24, 2, 1);
        // Cursor landed on row 23 but the shell only owns rows 1..=21.
        let s = fixup_sequence(&l, 23, 1, &[], true);
        assert!(s.contains("\x1b[24;1H\n\n"));
        assert!(s.contains("\x1b[22;1H\x1b[J"));
        assert!(s.ends_with("\x1b[21;1H"));
    }

    #[test]
    fn fallback_clears_reserved_rows() {
        let s = fallback_sequence(&layout(24, 2, 1), &[], true);
        assert!(s.contains("\x1b[22;1H\x1b[J\x1b[1;21r"));
        assert!(s.ends_with("\x1b8"));
    }

    #[test]
    fn restore_leaves_alt_screen_and_wipes_bar() {
        let s = restore_string(&layout(24, 2, 1), true);
        assert!(s.starts_with("\x1b[?1049l"));
        assert!(s.contains("\x1b[r\x1b[22;1H\x1b[J\x1b8"));
        assert!(!restore_string(&layout(24, 2, 1), false).contains("1049"));
    }

    #[test]
    fn explains_sshpass_exit_codes() {
        let session = Session {
            name: "web".into(),
            user: "root".into(),
            host: "11.12.13.14".into(),
            password: None,
            config: Config::default(),
        };
        assert!(
            sshpass_failure(5, &session)
                .unwrap()
                .contains("lssh passwd web")
        );
        assert!(sshpass_failure(6, &session).is_some());
        assert!(
            sshpass_failure(7, &session)
                .unwrap()
                .contains("ssh-keygen -R 11.12.13.14")
        );
        assert!(sshpass_failure(0, &session).is_none());
        assert!(sshpass_failure(255, &session).is_none());
    }
}
