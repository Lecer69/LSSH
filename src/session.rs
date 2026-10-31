use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::tty::IsTty;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::askpass::Askpass;
use crate::config::Config;
use crate::frame::{self, Canvas, Cell, Style};
use crate::input::{self, Button, Event, Mouse, MouseKind};
use crate::out;
use crate::panel::{self, Action, Panel};
use crate::proxy::{self, Proxy};
use crate::record::Recorder;
use crate::stats::{self, Stats};
use crate::store::{self, Server};
use crate::ui::{self, History, Theme};
use crate::vt;

/// Everything needed to open an interactive session.
pub struct Session {
    pub server: Server,
    pub config: Config,
    /// Save the session as an asciicast recording.
    pub record: bool,
    /// Go through this proxy (`-x`).
    pub proxy: Option<Proxy>,
}

/// Lines of remote history kept for the shell pane while the panel is open.
const SCROLLBACK: usize = 1000;
/// Shortest time between two repaints of the panel view.
const FRAME: Duration = Duration::from_millis(16);
/// The panel never gets narrower than this, nor the shell next to it.
const MIN_PANEL: u16 = 20;
const MIN_SHELL: u16 = 30;

/// Entry point: use the PTY proxy (stats bar and file panel) when attached
/// to a TTY, otherwise fall back to a plain `ssh` invocation.
pub fn run(session: &Session) -> Result<()> {
    let askpass = Askpass::new(&session.server, true)?;
    let proxy_options = proxy::ssh_options(session.proxy.as_ref())?;
    let target = session.server.target();

    let (cols, rows) = match crossterm::terminal::size() {
        Ok(size) if size.0 > 0 && size.1 > 0 => size,
        _ => return run_plain(session, &askpass, &proxy_options),
    };
    let layout = Layout::new(&session.config, cols, rows);
    // Without the bar and the panel there is nothing to add to plain ssh.
    if (layout.bar_h == 0 && !session.config.sftp) || !is_tty() {
        return run_plain(session, &askpass, &proxy_options);
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

    let command = build_command(session, &control_path, &askpass, &proxy_options);
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

    let theme = ui::Theme::from_config(&session.config);
    let address = session.server.address();
    let info = ui::BarInfo {
        name: &session.server.name,
        address: &address,
    };
    let render = |layout: &Layout, stats: Option<&Stats>, history: &History| {
        ui::render_bar(
            &info,
            stats,
            history,
            layout.cols as usize,
            layout.bar_h as usize,
            &session.config,
            &theme,
        )
    };

    let mut recording = None;
    let recorder = if session.record {
        match Recorder::create(&session.server.name, cols, layout.pty_rows()) {
            Ok((recorder, path)) => {
                recording = Some(path);
                Some(recorder)
            }
            Err(e) => {
                out::warn(format_args!("not recording: {e:#}"));
                None
            }
        }
    } else {
        None
    };

    // Remote stats poller (not needed when the bar is turned off).
    let (stats_tx, stats_rx) = mpsc::channel::<Stats>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let stats_path = control_path.clone();
    let stats_target = target.clone();
    let interval = session.config.update_interval_ms;
    let poll_stats = session.config.bar_height > 0;
    let stats_thread = thread::spawn(move || {
        let mut collector = stats::Collector::new();
        loop {
            match stop_rx.recv_timeout(Duration::from_millis(interval)) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            // Until the session has authenticated there is no master socket,
            // and ssh would open a fresh (failing) connection every tick.
            if !poll_stats || !stats_path.exists() {
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
    let screen = Arc::new(Mutex::new(Screen::new(
        layout,
        render(&layout, None, &History::default()),
        recorder,
        &theme,
    )));
    let guard = TermGuard(Arc::clone(&screen));

    // Set up the screen before any remote output is shown, so an early
    // prompt (host key, password) can't be wiped by the initial clear.
    if let Ok(mut s) = screen.lock() {
        let init = format!(
            "\x1b[2J\x1b[H\x1b]2;lssh · {}\x07\x1b[1;{}r",
            session.server.name,
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
    let mut history = History::default();
    let mut pending_fixup = false;
    let mut fixup_waits = 0u32;
    // A cursor report that arrived after we stopped waiting for it must not
    // be forwarded to the remote as keyboard input.
    let mut late_report: Option<Instant> = None;

    // The file panel (Ctrl+N), created the first time it is opened.
    let sftp_enabled = session.config.sftp;
    let mut hotkey = input::Hotkey::default();
    let mut files: Option<Files> = None;
    let mut want_open = false;
    let mut open_waits = 0u32;
    // Input typed right after Ctrl+N, before the panel was up.
    let mut replay: Vec<u8> = Vec::new();
    let mut watch = SessionWatch::new(master.as_raw_fd());
    let mut session_over = false;

    let result: Result<()> = (|| -> Result<()> {
        loop {
            let open = files.as_ref().is_some_and(|f| f.open);
            let mut fx = Effects::default();
            let mut to_remote: Vec<u8> = Vec::new();
            let mut after_close: Vec<u8> = Vec::new();

            let incoming = if open && !replay.is_empty() {
                Some(std::mem::take(&mut replay))
            } else {
                let wait = if open {
                    FRAME
                } else {
                    Duration::from_millis(50)
                };
                match input_rx.recv_timeout(wait) {
                    Ok(bytes) => Some(bytes),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };

            if open {
                let f = files.as_mut().expect("the panel is open");
                let tokens = match &incoming {
                    Some(bytes) => f.keys.feed(bytes),
                    None => f.keys.flush(),
                };
                if !tokens.is_empty() {
                    let (geometry, modes) = match screen.lock() {
                        Ok(s) => (s.view_geometry(), s.remote_modes()),
                        Err(_) => break,
                    };
                    for token in tokens {
                        if fx.close {
                            // Typed after closing the panel: for the shell.
                            after_close.extend_from_slice(&token.raw);
                        } else {
                            route(f, token, geometry, modes, &mut to_remote, &mut fx);
                        }
                    }
                }
            } else if let Some(mut bytes) = incoming {
                if let Some(deadline) = late_report
                    && (Instant::now() > deadline || vt::take_cursor_report(&mut bytes).is_some())
                {
                    late_report = None;
                }
                if want_open {
                    replay.extend_from_slice(&bytes);
                } else if sftp_enabled {
                    let (before, hit) = hotkey.split(&bytes);
                    to_remote = before;
                    if let Some(after) = hit {
                        want_open = true;
                        replay = after;
                    }
                } else {
                    to_remote = bytes;
                }
            }
            if !to_remote.is_empty() {
                writer.write_all(&to_remote)?;
                writer.flush()?;
            }

            if child.try_wait()?.is_some() {
                break;
            }
            // The remote shell is gone, but the ssh master waits for the
            // panel's SFTP sessions; close them so it can exit.
            if !session_over && watch.ended() {
                session_over = true;
                if let Some(f) = files.as_ref() {
                    f.panel.disconnect();
                    if f.panel.busy()
                        && let Ok(mut s) = screen.lock()
                    {
                        let note = "\r\n[lssh] finishing file transfers before closing (Ctrl+C stops them)\r\n";
                        let _ = s.write_remote(note.as_bytes());
                    }
                }
            }

            let mut got_stats = false;
            while let Ok(snapshot) = stats_rx.try_recv() {
                history.push(&snapshot);
                stats = Some(snapshot);
                got_stats = true;
            }

            if let Ok((new_cols, new_rows)) = crossterm::terminal::size()
                && (new_cols != layout.cols || new_rows != layout.rows)
                && new_cols > 0
                && new_rows > 0
            {
                layout = Layout::new(&session.config, new_cols, new_rows);
                if open {
                    fx.relayout = true;
                } else {
                    pending_fixup = true;
                }
            }

            let mut panel_changed = fx.redraw_panel;
            if let Some(f) = files.as_mut() {
                panel_changed |= f.panel.poll() || f.panel.dirty;
            }

            let Ok(mut s) = screen.lock() else {
                break;
            };

            // The panel is open: LSSH draws the whole shell area itself.
            if s.view.is_some() {
                let f = files.as_mut().expect("an open view has a panel");
                if got_stats {
                    s.bar = render(&s.layout, stats.as_ref(), &history);
                    s.bar_dirty = true;
                }
                let fits = panel_width(f.width, layout.cols).is_some();
                if fx.close || (fx.relayout && !fits) {
                    s.apply_layout(layout, render(&layout, stats.as_ref(), &history));
                    s.close_view()?;
                    f.open = false;
                    f.dragging = false;
                    resize_pty(
                        &*master,
                        &mut pty_size,
                        &mut s,
                        (layout.pty_rows(), layout.cols),
                    );
                    if !fits && !fx.close {
                        let _ = s.out.write_all(b"\x07");
                    }
                    drop(s);
                    after_close.extend_from_slice(&f.keys.take());
                    let (before, hit) = hotkey.split(&after_close);
                    if !before.is_empty() {
                        writer.write_all(&before)?;
                        writer.flush()?;
                    }
                    if let Some(rest) = hit {
                        want_open = true;
                        replay = rest;
                    }
                    continue;
                }
                if fx.relayout {
                    let width = panel_width(f.width, layout.cols).unwrap_or(MIN_PANEL);
                    f.width = width;
                    s.apply_layout(layout, render(&layout, stats.as_ref(), &history));
                    s.relayout_view(width);
                    let size = (layout.pty_rows(), layout.cols - width - 1);
                    resize_pty(&*master, &mut pty_size, &mut s, size);
                    panel_changed = true;
                }
                if fx.scroll != 0 || fx.reset_scroll {
                    s.scroll_view(fx.scroll, fx.reset_scroll);
                }
                if panel_changed {
                    let (width, rows) = s.view_geometry();
                    let (canvas, cursor) = f.panel.render(width, rows, f.focus_panel);
                    s.set_panel(canvas, cursor, f.focus_panel);
                }
                if s.view_dirty() || s.bar_dirty {
                    s.draw_view()?;
                }
                continue;
            }

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

                s.apply_layout(layout, render(&layout, stats.as_ref(), &history));
                let (leftover, answered) = s.fixup(&input_rx)?;
                if !answered {
                    late_report = Some(Instant::now() + Duration::from_secs(2));
                }
                // Resize the remote only after the region is in place, so
                // its SIGWINCH redraw lands in the right area.
                resize_pty(
                    &*master,
                    &mut pty_size,
                    &mut s,
                    (layout.pty_rows(), layout.cols),
                );
                drop(s);
                let leftover = if sftp_enabled && !want_open {
                    let (before, hit) = hotkey.split(&leftover);
                    if let Some(after) = hit {
                        want_open = true;
                        replay = after;
                    }
                    before
                } else {
                    leftover
                };
                if !leftover.is_empty() {
                    writer.write_all(&leftover)?;
                    writer.flush()?;
                }
            } else if want_open {
                if !s.filter.at_ground() && open_waits < 10 {
                    open_waits += 1;
                    continue;
                }
                open_waits = 0;
                want_open = false;
                let f = files
                    .get_or_insert_with(|| Files::new(session, &target, &control_path, &theme));
                let Some(width) = panel_width(f.width, layout.cols) else {
                    // Too narrow for a panel and a usable shell.
                    let _ = s.out.write_all(b"\x07");
                    let _ = s.out.flush();
                    drop(s);
                    if !replay.is_empty() {
                        writer.write_all(&std::mem::take(&mut replay))?;
                        writer.flush()?;
                    }
                    continue;
                };
                f.width = width;
                f.open = true;
                f.focus_panel = true;
                f.dragging = false;
                f.keys = input::Parser::new();
                s.open_view(width);
                let size = (layout.pty_rows(), layout.cols - width - 1);
                resize_pty(&*master, &mut pty_size, &mut s, size);
                let shell_dir = s.wide.callbacks().dir.clone();
                f.panel.opened(shell_dir.as_deref());
                let (canvas, cursor) = f.panel.render(width, layout.pty_rows(), true);
                s.set_panel(canvas, cursor, true);
                s.draw_view()?;
            } else {
                if got_stats {
                    let applied = s.layout;
                    s.bar = render(&applied, stats.as_ref(), &history);
                    s.bar_dirty = true;
                }
                s.paint_bar_if_ready()?;
                s.out.flush()?;
            }
        }
        Ok(())
    })();

    // Hand the screen back in the state the remote left it.
    if let Ok(mut s) = screen.lock()
        && s.view.is_some()
    {
        let _ = s.close_view();
    }
    let interrupted = files.as_ref().is_some_and(|f| f.panel.busy());
    drop(files);

    let _ = stop_tx.send(());
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader_thread.join();
    let _ = stats_thread.join();
    let _ = std::fs::remove_file(&control_path);
    let _ = std::fs::remove_dir(&control_dir);
    drop(guard);

    result?;
    if interrupted {
        out::warn("the session ended while files were still being transferred");
    }
    if let Some(path) = recording {
        out::success(format_args!("recorded to {}", path.display()));
        out::hint(format_args!(
            "replay with: asciinema play {}",
            path.display()
        ));
    }
    Ok(())
}

/// Tells when ssh's interactive session has ended, from the pty's mode:
/// ssh puts its terminal in raw mode once the session is up and restores
/// it when the session channel closes, even when the master connection
/// then stays up for other multiplexed sessions. (ssh also leaves raw mode
/// briefly for its `~C` prompt, so this is only a hint, never a reason to
/// quit by itself.)
struct SessionWatch {
    fd: Option<std::os::unix::io::RawFd>,
    seen_raw: bool,
    last_check: Instant,
}

impl SessionWatch {
    fn new(fd: Option<std::os::unix::io::RawFd>) -> SessionWatch {
        SessionWatch {
            fd,
            seen_raw: false,
            last_check: Instant::now(),
        }
    }

    fn ended(&mut self) -> bool {
        let Some(fd) = self.fd else {
            return false;
        };
        if self.last_check.elapsed() < Duration::from_millis(200) {
            return false;
        }
        self.last_check = Instant::now();
        // SAFETY: tcgetattr only fills in the struct.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
            return false;
        }
        let raw = t.c_lflag & libc::ICANON == 0;
        if raw {
            self.seen_raw = true;
        }
        self.seen_raw && !raw
    }
}

fn nz(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value.max(1)).expect("at least 1")
}

/// Resize the remote's pty (and the recording) if the size changed.
fn resize_pty(master: &dyn MasterPty, current: &mut (u16, u16), s: &mut Screen, size: (u16, u16)) {
    if size == *current {
        return;
    }
    *current = size;
    if let Some(recorder) = s.recorder.as_mut() {
        recorder.resize(size.1, size.0);
    }
    let _ = master.resize(PtySize {
        rows: size.0,
        cols: size.1,
        pixel_width: 0,
        pixel_height: 0,
    });
}

/// Width of the panel for a terminal `cols` wide, or `None` when the panel
/// and a usable shell do not both fit.
fn panel_width(preferred: u16, cols: u16) -> Option<u16> {
    let max = cols.checked_sub(MIN_SHELL + 1)?;
    (max >= MIN_PANEL).then(|| preferred.clamp(MIN_PANEL, max))
}

/// The file panel and the input state around it.
struct Files {
    panel: Panel,
    open: bool,
    /// Keys go to the panel (true) or the shell next to it.
    focus_panel: bool,
    /// Preferred width in columns.
    width: u16,
    keys: input::Parser,
    /// The border between panel and shell is being dragged.
    dragging: bool,
}

impl Files {
    fn new(
        session: &Session,
        target: &str,
        control_path: &std::path::Path,
        theme: &Theme,
    ) -> Files {
        Files {
            panel: Panel::new(
                &session.server.name,
                target,
                control_path,
                &session.config,
                theme,
            ),
            open: false,
            focus_panel: true,
            width: session.config.sftp_width,
            keys: input::Parser::new(),
            dragging: false,
        }
    }
}

/// What routing input asked for; applied under the screen lock.
#[derive(Debug, Default)]
struct Effects {
    close: bool,
    relayout: bool,
    redraw_panel: bool,
    /// Lines to scroll the shell pane back (positive) or forward.
    scroll: isize,
    /// Return the shell pane to the live screen.
    reset_scroll: bool,
}

/// Input modes the remote application asked for.
#[derive(Debug, Clone, Copy)]
struct RemoteModes {
    mouse: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
    bracketed_paste: bool,
    alt_screen: bool,
    app_cursor: bool,
}

/// Send one input token to the panel or to the shell next to it.
/// `geometry` is (panel width, shell rows).
fn route(
    f: &mut Files,
    token: input::Token,
    geometry: (u16, u16),
    modes: RemoteModes,
    to_remote: &mut Vec<u8>,
    fx: &mut Effects,
) {
    match token.event {
        // From the shell, Ctrl+N moves to the panel; in it, it closes.
        Event::Key(key) if key.is_ctrl('n') && !f.focus_panel => {
            f.focus_panel = true;
            fx.redraw_panel = true;
        }
        Event::Key(key) if f.focus_panel => {
            let action = f.panel.key(key);
            apply_action(f, action, fx);
            fx.redraw_panel = true;
        }
        Event::Key(_) | Event::Other => {
            if !f.focus_panel {
                to_remote.extend_from_slice(&token.raw);
                fx.reset_scroll = true;
            }
        }
        Event::Paste(text) => {
            if f.focus_panel {
                f.panel.paste(&text);
                fx.redraw_panel = true;
            } else {
                if modes.bracketed_paste {
                    to_remote.extend_from_slice(&token.raw);
                } else {
                    to_remote.extend_from_slice(text.as_bytes());
                }
                fx.reset_scroll = true;
            }
        }
        Event::Mouse(m) => route_mouse(f, m, geometry, modes, to_remote, fx),
    }
}

fn apply_action(f: &mut Files, action: Action, fx: &mut Effects) {
    match action {
        Action::None => {}
        Action::Close => fx.close = true,
        Action::FocusTerminal => f.focus_panel = false,
        Action::Resize(delta) => {
            f.width = (i32::from(f.width) + i32::from(delta)).clamp(1, 1000) as u16;
            fx.relayout = true;
        }
    }
}

fn route_mouse(
    f: &mut Files,
    m: Mouse,
    (panel_w, rows): (u16, u16),
    modes: RemoteModes,
    to_remote: &mut Vec<u8>,
    fx: &mut Effects,
) {
    if f.dragging {
        match m.kind {
            MouseKind::Drag | MouseKind::Move => {
                if m.col != panel_w {
                    f.width = m.col.max(1);
                    fx.relayout = true;
                }
            }
            _ => f.dragging = false,
        }
        return;
    }
    // The stats bar takes no clicks.
    if m.row >= rows {
        return;
    }
    if m.col < panel_w {
        if m.kind == MouseKind::Press && !f.focus_panel {
            f.focus_panel = true;
        }
        let action = f.panel.mouse(m);
        apply_action(f, action, fx);
        fx.redraw_panel = true;
    } else if m.col == panel_w {
        if m.kind == MouseKind::Press && m.button == Button::Left {
            f.dragging = true;
        }
    } else {
        let local = m.at(m.col - panel_w - 1, m.row);
        if m.kind == MouseKind::Press && f.focus_panel {
            f.focus_panel = false;
            fx.redraw_panel = true;
        }
        if modes.mouse != vt100::MouseProtocolMode::None {
            if let Some(bytes) = local.encode(modes.mouse, modes.encoding) {
                to_remote.extend_from_slice(&bytes);
            }
        } else if matches!(m.kind, MouseKind::ScrollUp | MouseKind::ScrollDown) {
            let up = m.kind == MouseKind::ScrollUp;
            if modes.alt_screen {
                // Full-screen apps without mouse support scroll with arrows.
                let key: &[u8] = match (up, modes.app_cursor) {
                    (true, true) => b"\x1bOA",
                    (true, false) => b"\x1b[A",
                    (false, true) => b"\x1bOB",
                    (false, false) => b"\x1b[B",
                };
                for _ in 0..3 {
                    to_remote.extend_from_slice(key);
                }
            } else {
                fx.scroll += if up { 3 } else { -3 };
            }
        }
    }
}

/// Remembers the directory the remote shell reports with OSC 7, so the
/// panel can open where the shell is.
#[derive(Debug, Default)]
struct Osc7 {
    dir: Option<String>,
}

impl vt100::Callbacks for Osc7 {
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.first() != Some(&&b"7"[..]) || params.len() < 2 {
            return;
        }
        let url: Vec<u8> = params[1..].join(&b';');
        let Ok(url) = std::str::from_utf8(&url) else {
            return;
        };
        if let Some(rest) = url.strip_prefix("file://")
            && let Some(slash) = rest.find('/')
        {
            self.dir = Some(panel::percent_decode(&rest[slash..]));
        }
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

/// The panel view: the file panel on the left and the remote screen,
/// redrawn from an emulator, on the right.
struct View {
    /// The remote screen at the width of the shell pane.
    narrow: vt100::Parser,
    panel_w: u16,
    panel: Canvas,
    /// Cursor position inside the panel (prompts).
    panel_cursor: Option<(u16, u16)>,
    focus_panel: bool,
    /// The frame on screen, for diffing.
    prev: Option<Canvas>,
    dirty: bool,
    last_draw: Instant,
    /// Input modes currently set on the real terminal.
    app_cursor: bool,
    app_keypad: bool,
    any_motion: bool,
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
    recorder: Option<Recorder>,
    /// Shadow copy of the remote screen at the full width; it is what the
    /// terminal gets back when the panel closes.
    wide: vt100::Parser<Osc7>,
    /// The terminal's main screen is out of date: redraw it from `wide`
    /// when the remote leaves the alt screen.
    resync_main: bool,
    resync_due: bool,
    view: Option<View>,
    /// Border styles: shell focused, panel focused.
    border: (Style, Style),
}

impl Screen {
    fn new(layout: Layout, bar: Vec<String>, recorder: Option<Recorder>, theme: &Theme) -> Self {
        let border = if theme.color {
            (
                Style::fg(frame::rgb(theme.frame)),
                Style::fg(frame::rgb(theme.cpu)),
            )
        } else {
            (Style::default().dim(), Style::default().bold())
        };
        Screen {
            out: io::stdout(),
            filter: vt::Filter::new(layout.pty_rows()),
            layout,
            bar,
            bar_dirty: true,
            needs_fixup: false,
            recorder,
            wide: vt100::Parser::new_with_callbacks(
                nz(layout.pty_rows()),
                nz(layout.cols),
                SCROLLBACK,
                Osc7::default(),
            ),
            resync_main: false,
            resync_due: false,
            view: None,
            border,
        }
    }

    /// Forward remote output, repainting the bar right away if it was erased.
    fn write_remote(&mut self, data: &[u8]) -> io::Result<()> {
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.output(data);
        }
        self.wide.process(data);
        if let Some(view) = self.view.as_mut() {
            view.narrow.process(data);
            view.dirty = true;
            if view.last_draw.elapsed() >= FRAME {
                self.draw_view()?;
            }
            return Ok(());
        }
        let mut filtered = Vec::with_capacity(data.len() + 16);
        let ev = self.filter.feed(data, &mut filtered);
        self.out.write_all(&filtered)?;
        self.bar_dirty |= ev.repaint;
        if ev.fixup && self.resync_main {
            self.resync_due = true;
        } else {
            self.needs_fixup |= ev.fixup;
        }
        if self.resync_due && self.filter.at_ground() {
            self.resync_due = false;
            self.resync_main = false;
            return self.restore_from_emulator();
        }
        self.paint_bar_if_ready()?;
        self.out.flush()
    }

    /// Redraw the bar if needed and the remote stream is between sequences.
    /// The caller flushes.
    fn paint_bar_if_ready(&mut self) -> io::Result<()> {
        if self.bar_dirty && self.filter.at_ground() && self.layout.bar_h > 0 {
            // The panel view keeps autowrap off.
            let autowrap = self.filter.autowrap() && self.view.is_none();
            let text = bar_string(&self.layout, &self.bar, autowrap);
            self.out.write_all(text.as_bytes())?;
            self.bar_dirty = false;
        }
        Ok(())
    }

    /// Take a new layout (and its bar) into use for the emulators too.
    fn apply_layout(&mut self, layout: Layout, bar: Vec<String>) {
        self.layout = layout;
        self.filter.set_pty_rows(layout.pty_rows());
        self.bar = bar;
        self.bar_dirty = true;
        self.wide.set_size(nz(layout.pty_rows()), nz(layout.cols));
    }

    /// (panel width, shell rows) of the view.
    fn view_geometry(&self) -> (u16, u16) {
        let width = self.view.as_ref().map_or(0, |v| v.panel_w);
        (width, self.layout.pty_rows())
    }

    fn remote_modes(&self) -> RemoteModes {
        let screen = match &self.view {
            Some(view) => view.narrow.screen(),
            None => self.wide.screen(),
        };
        RemoteModes {
            mouse: screen.mouse_protocol_mode(),
            encoding: screen.mouse_protocol_encoding(),
            bracketed_paste: screen.bracketed_paste(),
            alt_screen: screen.alternate_screen(),
            app_cursor: screen.application_cursor(),
        }
    }

    fn view_dirty(&self) -> bool {
        self.view.as_ref().is_some_and(|v| v.dirty)
    }

    fn set_panel(&mut self, panel: Canvas, cursor: Option<(u16, u16)>, focus_panel: bool) {
        if let Some(view) = self.view.as_mut() {
            view.panel = panel;
            view.panel_cursor = cursor;
            view.focus_panel = focus_panel;
            view.dirty = true;
        }
    }

    /// Scroll the shell pane through its history.
    fn scroll_view(&mut self, delta: isize, reset: bool) {
        if let Some(view) = self.view.as_mut() {
            let screen = view.narrow.screen_mut();
            let current = screen.scrollback();
            let base = if reset { 0 } else { current as isize };
            let next = (base + delta).max(0) as usize;
            if next != current {
                screen.set_scrollback(next);
                view.dirty = true;
            }
        }
    }

    /// Switch to the panel view. The terminal goes to the alt screen (so
    /// the main screen and its scrollback stay as they are) with mouse
    /// reporting and bracketed paste on.
    fn open_view(&mut self, panel_w: u16) {
        let rows = self.layout.pty_rows();
        let shell_cols = self.layout.cols.saturating_sub(panel_w + 1);
        let mut narrow = vt100::Parser::new(nz(rows), nz(shell_cols), 0);
        *narrow.screen_mut() = self.wide.screen().clone();
        narrow.screen_mut().set_size(nz(rows), nz(shell_cols));
        narrow.screen_mut().set_scrollback(0);
        let screen = self.wide.screen();
        let mut init = String::new();
        if !self.filter.alt_screen() {
            init.push_str("\x1b[?1049h");
        }
        // The filter sees no remote output until the view closes (and then
        // starts over); it must not hold the bar back meanwhile.
        let alt = self.filter.alt_screen();
        self.filter.reset(alt);
        init.push_str("\x1b[0m\x1b[?6l\x1b[?7l\x1b[r\x1b[2J\x1b[?25l");
        init.push_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h");
        let _ = self.out.write_all(init.as_bytes());
        self.view = Some(View {
            narrow,
            panel_w,
            panel: Canvas::new(panel_w, rows, Style::default()),
            panel_cursor: None,
            focus_panel: true,
            prev: None,
            dirty: true,
            last_draw: Instant::now(),
            app_cursor: screen.application_cursor(),
            app_keypad: screen.application_keypad(),
            any_motion: false,
        });
        self.bar_dirty = true;
    }

    /// Apply a new panel width or terminal size to the open view.
    fn relayout_view(&mut self, panel_w: u16) {
        let rows = self.layout.pty_rows();
        let shell_cols = self.layout.cols.saturating_sub(panel_w + 1);
        if let Some(view) = self.view.as_mut() {
            view.narrow.set_size(nz(rows), nz(shell_cols));
            view.panel_w = panel_w;
            view.panel = Canvas::new(panel_w, rows, Style::default());
            view.prev = None;
            view.dirty = true;
            let _ = self.out.write_all(b"\x1b[0m\x1b[r\x1b[2J");
            self.bar_dirty = true;
        }
    }

    /// Leave the panel view and give the terminal back to the remote.
    fn close_view(&mut self) -> io::Result<()> {
        if self.view.take().is_none() {
            return Ok(());
        }
        let alt = self.wide.screen().alternate_screen();
        let mut text = String::from("\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1006l\x1b[?1005l");
        if !alt {
            text.push_str("\x1b[?1049l");
        }
        self.out.write_all(text.as_bytes())?;
        self.filter.reset(alt);
        // The main screen the terminal saved is stale; fix it when the
        // remote returns to it.
        self.resync_main = alt;
        self.resync_due = false;
        self.restore_from_emulator()
    }

    /// Redraw the shell area from the full-width emulator and restore the
    /// remote's input modes, so passthrough can continue from there.
    fn restore_from_emulator(&mut self) -> io::Result<()> {
        let screen = self.wide.screen();
        let mut text = format!("\x1b[0m\x1b[?6l\x1b[?7h\x1b[1;{}r", self.layout.pty_rows());
        text.push_str(&screen.contents_formatted());
        text.push_str(&screen.input_mode_formatted());
        self.out.write_all(text.as_bytes())?;
        self.bar_dirty = true;
        self.paint_bar_if_ready()?;
        self.out.flush()
    }

    /// Draw the panel view (only what changed since the last frame).
    fn draw_view(&mut self) -> io::Result<()> {
        let layout = self.layout;
        let (idle_border, focus_border) = self.border;
        let Some(view) = self.view.as_mut() else {
            return Ok(());
        };
        let rows = layout.pty_rows();
        let cols = layout.cols;
        let pw = view.panel_w.min(cols);
        let shell_cols = cols.saturating_sub(pw + 1);

        let mut frame = Canvas::new(cols, rows, Style::default());
        frame.blit(&view.panel, 0, 0);
        let border = if view.focus_panel {
            focus_border
        } else {
            idle_border
        };
        for row in 0..rows {
            frame.set(
                row,
                pw,
                Cell {
                    ch: '│',
                    extra: None,
                    style: border,
                    width: 1,
                },
            );
        }
        let screen = view.narrow.screen();
        for row in 0..rows {
            for col in 0..shell_cols {
                if let Some(cell) = screen.cell(row, col) {
                    frame.set(row, pw + 1 + col, Cell::from_vt(cell));
                }
            }
        }
        let scrolled = screen.scrollback();
        if scrolled > 0 {
            let label = format!(" ↑ {scrolled} ");
            let w = frame::width(&label) as u16;
            if w < shell_cols {
                frame.text(0, cols - w, w, &label, Style::default().inverse());
            }
        }

        let mut text = String::new();
        frame::diff(view.prev.as_ref(), &frame, &mut text);

        // Mirror the remote's key modes, so keys typed into the shell pane
        // reach it in the form it expects.
        if screen.application_cursor() != view.app_cursor {
            view.app_cursor = screen.application_cursor();
            text.push_str(if view.app_cursor {
                "\x1b[?1h"
            } else {
                "\x1b[?1l"
            });
        }
        if screen.application_keypad() != view.app_keypad {
            view.app_keypad = screen.application_keypad();
            text.push_str(if view.app_keypad { "\x1b=" } else { "\x1b>" });
        }
        let any = screen.mouse_protocol_mode() == vt100::MouseProtocolMode::AnyMotion;
        if any != view.any_motion {
            view.any_motion = any;
            text.push_str(if any { "\x1b[?1003h" } else { "\x1b[?1003l" });
        }

        let cursor = if view.focus_panel {
            view.panel_cursor
        } else if screen.hide_cursor() || scrolled > 0 || shell_cols == 0 {
            None
        } else {
            let (row, col) = screen.cursor_position();
            Some((row, pw + 1 + col.min(shell_cols - 1)))
        };
        match cursor {
            Some((row, col)) => text.push_str(&format!("\x1b[{};{}H\x1b[?25h", row + 1, col + 1)),
            None => text.push_str("\x1b[?25l"),
        }
        self.out.write_all(text.as_bytes())?;
        view.prev = Some(frame);
        view.dirty = false;
        view.last_draw = Instant::now();
        self.paint_bar_if_ready()?;
        self.out.flush()
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

/// Plain pass-through used when there is no TTY, or neither the bar nor the
/// file panel is enabled.
fn run_plain(session: &Session, askpass: &Askpass, proxy_options: &[String]) -> Result<()> {
    if session.record {
        out::warn("recording needs the stats bar session, so this one is not recorded");
    }
    let status = askpass
        .apply(&mut std::process::Command::new("ssh"))
        .args(session.server.ssh_options())
        .args(proxy_options)
        .arg(session.server.target())
        .status()
        .context("failed to launch ssh (is openssh installed?)")?;
    // Other codes are just the remote shell's last exit status.
    if status.code() == Some(255) {
        bail!("ssh could not connect to {}", session.server.address());
    }
    Ok(())
}

/// Build the interactive `ssh` command.
///
/// `ControlMaster=yes` turns this session into a multiplexing master so the
/// stats poller can reuse its authenticated connection (and its proxy).
fn build_command(
    session: &Session,
    control_path: &std::path::Path,
    askpass: &Askpass,
    proxy_options: &[String],
) -> CommandBuilder {
    let mut command = CommandBuilder::new("ssh");
    command.args([
        "-o".to_string(),
        "ControlMaster=yes".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", control_path.display()),
        "-o".to_string(),
        "ServerAliveInterval=30".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
    ]);
    command.args(session.server.ssh_options());
    command.args(proxy_options);
    command.arg(session.server.target());
    for (key, value) in askpass.vars() {
        command.env(key, value);
    }
    command
}

fn create_control_dir() -> Result<PathBuf> {
    let dir = store::runtime_dir().join(format!("lssh-{}", std::process::id()));
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    store::create_private_dir(&dir)?;
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
    // Mouse reporting, bracketed paste and key modes set by the remote (or
    // the file panel) must not outlive the session.
    out.push_str("\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1006l\x1b[?1005l\x1b[?2004l\x1b[?1l\x1b>");
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
        // The panel view always runs on the alt screen.
        let alt = s.filter.alt_screen() || s.view.is_some();
        let text = restore_string(&s.layout, alt);
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
    fn panel_width_leaves_room_for_the_shell() {
        assert_eq!(panel_width(36, 120), Some(36));
        assert_eq!(panel_width(100, 120), Some(89));
        assert_eq!(panel_width(5, 120), Some(MIN_PANEL));
        assert_eq!(panel_width(36, 51), Some(20));
        assert_eq!(panel_width(36, 50), None);
        assert_eq!(panel_width(36, 10), None);
    }

    #[test]
    fn osc7_reports_the_shell_directory() {
        let mut p = vt100::Parser::new_with_callbacks(nz(5), nz(20), 0, Osc7::default());
        p.process(b"\x1b]7;file://host/tmp/a%20b\x07");
        assert_eq!(p.callbacks().dir.as_deref(), Some("/tmp/a b"));
        p.process(b"\x1b]7;file://host/x;y\x1b\\");
        assert_eq!(p.callbacks().dir.as_deref(), Some("/x;y"));
        p.process(b"\x1b]2;title\x07");
        assert_eq!(p.callbacks().dir.as_deref(), Some("/x;y"));
    }

    #[test]
    fn mouse_routing_between_panel_border_and_shell() {
        let modes = |mouse| RemoteModes {
            mouse,
            encoding: vt100::MouseProtocolEncoding::Sgr,
            bracketed_paste: false,
            alt_screen: false,
            app_cursor: false,
        };
        let press = |col| {
            let mut p = input::Parser::new();
            let token = p
                .feed(format!("\x1b[<0;{};3M", col + 1).as_bytes())
                .remove(0);
            let Event::Mouse(m) = token.event else {
                panic!("expected mouse")
            };
            m
        };
        let mut f = test_files();
        let mut out = Vec::new();
        let mut fx = Effects::default();
        // A click in the shell pane moves focus there and reaches an app
        // that asked for the mouse, at pane-relative coordinates.
        route_mouse(
            &mut f,
            press(50),
            (30, 20),
            modes(vt100::MouseProtocolMode::PressRelease),
            &mut out,
            &mut fx,
        );
        assert!(!f.focus_panel);
        assert_eq!(out, b"\x1b[<0;20;3M");
        // Without mouse mode, nothing is sent.
        out.clear();
        route_mouse(
            &mut f,
            press(50),
            (30, 20),
            modes(vt100::MouseProtocolMode::None),
            &mut out,
            &mut fx,
        );
        assert!(out.is_empty());
        // The border starts a drag; motion resizes.
        route_mouse(
            &mut f,
            press(30),
            (30, 20),
            modes(vt100::MouseProtocolMode::None),
            &mut out,
            &mut fx,
        );
        assert!(f.dragging);
        let mut p = input::Parser::new();
        let Event::Mouse(drag) = p.feed(b"\x1b[<32;41;3M").remove(0).event else {
            panic!()
        };
        route_mouse(
            &mut f,
            drag,
            (30, 20),
            modes(vt100::MouseProtocolMode::None),
            &mut out,
            &mut fx,
        );
        assert_eq!((f.width, fx.relayout), (40, true));
        // A click in the panel focuses it again.
        f.dragging = false;
        route_mouse(
            &mut f,
            press(5),
            (30, 20),
            modes(vt100::MouseProtocolMode::None),
            &mut out,
            &mut fx,
        );
        assert!(f.focus_panel);
    }

    fn test_files() -> Files {
        let theme = Theme::from_name("btop");
        let config = Config::default();
        Files {
            panel: Panel::new(
                "t",
                "nobody@localhost",
                std::path::Path::new("/nonexistent/sock"),
                &config,
                &theme,
            ),
            open: true,
            focus_panel: true,
            width: 30,
            keys: input::Parser::new(),
            dragging: false,
        }
    }

    #[test]
    fn restore_leaves_alt_screen_and_wipes_bar() {
        let s = restore_string(&layout(24, 2, 1), true);
        assert!(s.starts_with("\x1b[?1049l"));
        assert!(s.contains("\x1b[r\x1b[22;1H\x1b[J\x1b8"));
        assert!(!restore_string(&layout(24, 2, 1), false).contains("1049"));
    }
}
