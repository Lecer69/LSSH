//! The SFTP file panel on the left of a session (toggled with Ctrl+N).
//!
//! This module holds the browser state and draws it into a [`Canvas`]; the
//! session decides where it goes on screen and routes input to it. All
//! remote work happens on [`files::Worker`] threads.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::files::{Entry, Event, Kind, Progress, Summary, Task, Worker, human_size};
use crate::frame::{Canvas, Style, rgb, truncate, width};
use crate::input::{Button, Key, KeyCode, Mouse, MouseKind};
use crate::sftp;
use crate::store;
use crate::ui::{Rgb, Theme};

/// What the session should do after the panel handled some input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
    FocusTerminal,
    /// Make the panel this many columns wider (or narrower).
    Resize(i16),
}

/// How long status messages stay up.
const MESSAGE_TTL: Duration = Duration::from_secs(8);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const WHEEL_STEP: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Info,
    Ok,
    Error,
}

struct Message {
    text: String,
    tone: Tone,
    at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Upload,
    Download,
    Delete,
}

struct Job {
    id: u64,
    kind: JobKind,
    /// What the job is about, for its status line.
    label: String,
    /// Where downloads go, for the final message.
    dest: Option<String>,
    progress: Progress,
    started: Option<Instant>,
}

/// A small line editor for the panel's prompts.
#[derive(Debug, Clone, Default)]
struct LineEdit {
    text: Vec<char>,
    cursor: usize,
}

impl LineEdit {
    fn new(text: &str) -> LineEdit {
        let text: Vec<char> = text.chars().collect();
        LineEdit {
            cursor: text.len(),
            text,
        }
    }

    fn value(&self) -> String {
        self.text.iter().collect()
    }

    fn insert(&mut self, s: &str) {
        for c in s.chars().filter(|c| !c.is_control()) {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Returns false for keys it does not use.
    fn key(&mut self, key: &Key) -> bool {
        match key.code {
            KeyCode::Char(c) if key.ctrl => match c {
                'a' => self.cursor = 0,
                'e' => self.cursor = self.text.len(),
                'u' => {
                    self.text.drain(..self.cursor);
                    self.cursor = 0;
                }
                'k' => self.text.truncate(self.cursor),
                'w' => {
                    let mut start = self.cursor;
                    while start > 0 && self.text[start - 1] == ' ' {
                        start -= 1;
                    }
                    while start > 0 && !matches!(self.text[start - 1], ' ' | '/') {
                        start -= 1;
                    }
                    if start == self.cursor && start > 0 {
                        start -= 1;
                    }
                    self.text.drain(start..self.cursor);
                    self.cursor = start;
                }
                _ => return false,
            },
            KeyCode::Char(c) if !key.alt => {
                self.text.insert(self.cursor, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.text.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.text.len() => {
                self.text.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.text.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    /// The part that fits in `cols` columns, and the cursor column in it.
    fn view(&self, cols: usize) -> (String, usize) {
        let cols = cols.max(2);
        let widths: Vec<usize> = self.text.iter().map(|c| width(&c.to_string())).collect();
        // Scroll so the cursor (and one cell after it) is visible.
        let mut start = 0;
        let span = |from: usize, to: usize| widths[from..to].iter().sum::<usize>();
        while start < self.cursor && span(start, self.cursor) + 1 > cols {
            start += 1;
        }
        let mut shown = String::new();
        let mut used = 0;
        for (i, c) in self.text.iter().enumerate().skip(start) {
            if used + widths[i] > cols {
                break;
            }
            shown.push(*c);
            used += widths[i];
        }
        (shown, span(start, self.cursor))
    }
}

#[derive(Debug, Clone)]
enum InputKind {
    NewFile,
    NewDir,
    Rename { from: String },
    Upload,
    Download { paths: Vec<String> },
    Goto,
}

impl InputKind {
    fn title(&self) -> &'static str {
        match self {
            InputKind::NewFile => "New file",
            InputKind::NewDir => "New folder",
            InputKind::Rename { .. } => "Rename",
            InputKind::Upload => "Upload",
            InputKind::Download { .. } => "Download",
            InputKind::Goto => "Go to",
        }
    }

    /// Prompts that take a local path get Tab completion.
    fn local_path(&self) -> Option<bool> {
        match self {
            InputKind::Upload => Some(false),
            InputKind::Download { .. } => Some(true),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
enum ConfirmKind {
    /// First delete confirmation; the second one asks to type the name.
    Delete {
        paths: Vec<String>,
    },
    OverwriteUpload {
        sources: Vec<PathBuf>,
    },
    OverwriteDownload {
        paths: Vec<String>,
        dir: PathBuf,
    },
}

enum Dialog {
    Input {
        kind: InputKind,
        lines: Vec<String>,
        edit: LineEdit,
        error: Option<String>,
    },
    Confirm {
        kind: ConfirmKind,
        title: String,
        lines: Vec<String>,
        ok: &'static str,
        danger: bool,
        /// Which button has focus; Cancel by default.
        on_ok: bool,
    },
    /// Counting what a delete would remove.
    Scanning { id: u64, paths: Vec<String> },
    /// Last delete confirmation: type the name (or `delete`) to go ahead.
    TypeToConfirm {
        paths: Vec<String>,
        expected: String,
        lines: Vec<String>,
        edit: LineEdit,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    NewFile,
    NewDir,
    Upload,
    Download,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Hit {
    Row(usize),
    Crumb(String),
    Button(Btn),
    Close,
    Help,
    CancelJob,
    DialogOk,
    DialogCancel,
    /// Swallows clicks (inside a dialog or the help screen).
    Inert,
}

struct HitBox {
    row: u16,
    cols: std::ops::Range<u16>,
    hit: Hit,
}

/// One row of the listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Parent,
    Entry(usize),
}

#[derive(Debug, Clone, Copy)]
struct Palette {
    color: bool,
    text: Rgb,
    dim: Rgb,
    accent: Rgb,
    dir: Rgb,
    link: Rgb,
    exec: Rgb,
    frame: Rgb,
    warn: Rgb,
    crit: Rgb,
}

impl Palette {
    fn new(theme: &Theme) -> Palette {
        Palette {
            color: theme.color,
            text: theme.name,
            dim: theme.label,
            accent: theme.cpu,
            dir: theme.disk,
            link: theme.mem,
            exec: theme.net_down,
            frame: theme.frame,
            warn: theme.warn,
            crit: theme.crit,
        }
    }

    fn fg(&self, c: Rgb) -> Style {
        if self.color {
            Style::fg(rgb(c))
        } else {
            Style::default()
        }
    }

    fn on(&self, style: Style, c: Rgb) -> Style {
        if self.color { style.on(rgb(c)) } else { style }
    }

    /// A filled badge or button.
    fn badge(&self, fg: Rgb, bg: Rgb) -> Style {
        if self.color {
            Style::fg(rgb(fg)).on(rgb(bg))
        } else {
            Style::default().inverse()
        }
    }

    fn selection(&self, base: Style, focused: bool) -> Style {
        if !self.color {
            return if focused {
                base.inverse()
            } else {
                base.underline()
            };
        }
        let bg = if focused {
            self.frame
        } else {
            mix(self.frame, (0, 0, 0), 0.5)
        };
        let mut style = base.on(rgb(bg));
        if focused {
            style.bold = true;
        }
        style
    }

    fn tone(&self, tone: Tone) -> Style {
        match tone {
            Tone::Info => self.fg(self.text),
            Tone::Ok => self.fg(self.accent),
            Tone::Error => self.fg(self.crit).bold(),
        }
    }
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let blend = |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
    (blend(a.0, b.0), blend(a.1, b.1), blend(a.2, b.2))
}

pub struct Panel {
    server: String,
    nav: Worker,
    jobs: Worker,
    events: Receiver<Event>,
    palette: Palette,
    download_dir: PathBuf,

    cwd: Option<String>,
    entries: Vec<Entry>,
    show_hidden: bool,
    selected: usize,
    scroll: usize,
    marked: BTreeSet<String>,
    loading: bool,
    waiting_login: bool,
    list_error: Option<String>,
    list_seq: u64,
    /// The directory the remote shell last reported (OSC 7).
    shell_dir: Option<String>,
    /// Folder sizes computed with `s`, by path.
    dir_sizes: HashMap<String, Summary>,
    /// Size scans in flight: id -> folder path.
    size_scans: HashMap<u64, String>,

    dialog: Option<Dialog>,
    help: bool,
    message: Option<Message>,
    queue: Vec<Job>,
    next_id: u64,

    hits: Vec<HitBox>,
    list_rows: usize,
    last_click: Option<(Instant, usize)>,
    /// Needs to be drawn again.
    pub dirty: bool,
}

impl Panel {
    pub fn new(
        server: &str,
        target: &str,
        control_path: &Path,
        config: &Config,
        theme: &Theme,
    ) -> Panel {
        let (tx, events) = mpsc::channel();
        let nav = Worker::spawn(target.to_string(), control_path.to_path_buf(), tx.clone());
        let jobs = Worker::spawn(target.to_string(), control_path.to_path_buf(), tx);
        Panel {
            server: server.to_string(),
            nav,
            jobs,
            events,
            palette: Palette::new(theme),
            download_dir: default_download_dir(&config.download_dir),
            cwd: None,
            entries: Vec::new(),
            show_hidden: config.sftp_hidden,
            selected: 0,
            scroll: 0,
            marked: BTreeSet::new(),
            loading: false,
            waiting_login: false,
            list_error: None,
            list_seq: 0,
            shell_dir: None,
            dir_sizes: HashMap::new(),
            size_scans: HashMap::new(),
            dialog: None,
            help: false,
            message: None,
            queue: Vec::new(),
            next_id: 1,
            hits: Vec::new(),
            list_rows: 10,
            last_click: None,
            dirty: true,
        }
    }

    /// Called whenever the panel is shown. `shell_dir` is the remote
    /// shell's working directory, when it reports one.
    pub fn opened(&mut self, shell_dir: Option<&str>) {
        let moved = shell_dir.is_some() && shell_dir != self.shell_dir.as_deref();
        if let Some(dir) = shell_dir {
            self.shell_dir = Some(dir.to_string());
        }
        if self.cwd.is_none() && !self.loading {
            self.list(shell_dir.unwrap_or(""), None);
        } else if moved && let Some(dir) = shell_dir {
            self.list(dir, None);
        }
        self.dirty = true;
    }

    /// Transfers or deletes are queued or running.
    pub fn busy(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Close the SFTP connections once queued work is done. The ssh master
    /// connection only exits after every session multiplexed over it has
    /// closed, so this lets the session end.
    pub fn disconnect(&self) {
        self.nav.disconnect();
        self.jobs.disconnect();
    }

    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn say(&mut self, tone: Tone, text: impl Into<String>) {
        self.message = Some(Message {
            text: text.into(),
            tone,
            at: Instant::now(),
        });
        self.dirty = true;
    }

    fn list(&mut self, path: &str, select: Option<String>) {
        self.list_seq += 1;
        self.loading = true;
        self.nav.send(Task::List {
            seq: self.list_seq,
            path: path.to_string(),
            select,
        });
        self.dirty = true;
    }

    fn refresh(&mut self) {
        let Some(cwd) = self.cwd.clone() else {
            self.list("", None);
            return;
        };
        let keep = self.selected_entry().map(|e| e.name.clone());
        self.list(&cwd, keep);
    }

    /// Handle worker results; returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            self.event(event);
            changed = true;
        }
        if let Some(m) = &self.message
            && m.at.elapsed() > MESSAGE_TTL
        {
            self.message = None;
            changed = true;
        }
        if changed {
            self.dirty = true;
        }
        changed
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Waiting => self.waiting_login = true,
            Event::Listed {
                seq,
                path,
                select,
                result,
            } => {
                if seq != self.list_seq {
                    return;
                }
                self.loading = false;
                self.waiting_login = false;
                match result {
                    Ok(mut entries) => {
                        sort_entries(&mut entries);
                        let same_dir = self.cwd.as_deref() == Some(path.as_str());
                        let previous = self.selected_entry().map(|e| e.name.clone());
                        if !same_dir {
                            self.marked.clear();
                            self.scroll = 0;
                        }
                        self.cwd = Some(path);
                        self.entries = entries;
                        self.list_error = None;
                        // Drop marks of entries that are gone.
                        let names: BTreeSet<&str> =
                            self.entries.iter().map(|e| e.name.as_str()).collect();
                        self.marked.retain(|m| names.contains(m.as_str()));
                        let want = select.or(if same_dir { previous } else { None });
                        self.selected = want.and_then(|name| self.row_of(&name)).unwrap_or(0);
                        self.clamp_selection();
                    }
                    Err(e) => {
                        if self.cwd.is_none() {
                            self.list_error = Some(e.clone());
                        }
                        self.say(Tone::Error, e);
                    }
                }
            }
            Event::Changed {
                dir,
                select,
                result,
            } => match result {
                Ok(text) => {
                    self.say(Tone::Ok, text);
                    if self.cwd.as_deref() == Some(dir.as_str()) {
                        self.list(&dir, select);
                    }
                }
                Err(e) => self.say(Tone::Error, e),
            },
            Event::Scanned { id, result } => {
                if let Some(path) = self.size_scans.remove(&id) {
                    match result {
                        Ok(summary) => {
                            self.dir_sizes.insert(path, summary);
                        }
                        Err(e) => self.say(Tone::Error, e),
                    }
                    return;
                }
                let Some(Dialog::Scanning { id: want, paths }) = &self.dialog else {
                    return;
                };
                if *want != id {
                    return;
                }
                let paths = paths.clone();
                match result {
                    Ok(summary) => self.confirm_delete(paths, Some(summary)),
                    Err(e) => {
                        self.dialog = None;
                        self.say(Tone::Error, e);
                    }
                }
            }
            Event::Started { id } => {
                if let Some(job) = self.queue.iter_mut().find(|j| j.id == id) {
                    job.started = Some(Instant::now());
                }
            }
            Event::Progress { id, progress } => {
                if let Some(job) = self.queue.iter_mut().find(|j| j.id == id) {
                    job.progress = progress;
                    job.started.get_or_insert_with(Instant::now);
                }
            }
            Event::Finished { id, dir, result } => {
                let job = self
                    .queue
                    .iter()
                    .position(|j| j.id == id)
                    .map(|i| self.queue.remove(i));
                match result {
                    Ok(mut text) => {
                        if let Some(dest) = job.as_ref().and_then(|j| j.dest.as_ref()) {
                            text.push_str(&format!(" to {dest}"));
                        }
                        self.say(Tone::Ok, text);
                    }
                    Err(e) => self.say(Tone::Error, e),
                }
                if let Some(dir) = dir
                    && self.cwd.as_deref() == Some(dir.as_str())
                {
                    self.refresh();
                }
            }
        }
    }

    // ----- listing helpers -------------------------------------------------

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::with_capacity(self.entries.len() + 1);
        if self.cwd.as_deref().is_some_and(|c| c != "/") {
            rows.push(Row::Parent);
        }
        rows.extend(
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| self.show_hidden || !e.is_hidden())
                .map(|(i, _)| Row::Entry(i)),
        );
        rows
    }

    fn row_of(&self, name: &str) -> Option<usize> {
        self.rows()
            .iter()
            .position(|r| matches!(r, Row::Entry(i) if self.entries[*i].name == name))
    }

    fn current_row(&self) -> Option<Row> {
        self.rows().get(self.selected).copied()
    }

    fn selected_entry(&self) -> Option<&Entry> {
        match self.current_row()? {
            Row::Entry(i) => self.entries.get(i),
            Row::Parent => None,
        }
    }

    fn clamp_selection(&mut self) {
        let count = self.rows().len();
        self.selected = self.selected.min(count.saturating_sub(1));
    }

    fn move_by(&mut self, delta: isize) {
        let count = self.rows().len();
        if count == 0 {
            return;
        }
        let next = (self.selected as isize + delta).clamp(0, count as isize - 1);
        self.selected = next as usize;
        self.dirty = true;
    }

    /// Paths the next operation applies to: the marked entries, or the
    /// selected one.
    fn targets(&self) -> Vec<Entry> {
        if !self.marked.is_empty() {
            return self
                .entries
                .iter()
                .filter(|e| self.marked.contains(&e.name))
                .cloned()
                .collect();
        }
        self.selected_entry().cloned().into_iter().collect()
    }

    fn remote_path(&self, name: &str) -> Option<String> {
        self.cwd.as_ref().map(|cwd| sftp::join(cwd, name))
    }

    fn go_up(&mut self) {
        let Some(cwd) = self.cwd.clone() else {
            return;
        };
        if cwd == "/" {
            return;
        }
        let name = sftp::file_name(&cwd).to_string();
        self.list(&sftp::parent(&cwd), Some(name));
    }

    /// Enter the selected folder (or download the selected file).
    fn activate(&mut self) {
        match self.current_row() {
            Some(Row::Parent) => self.go_up(),
            Some(Row::Entry(i)) => {
                let entry = self.entries[i].clone();
                if entry.is_dir_like() {
                    if let Some(path) = self.remote_path(&entry.name) {
                        self.list(&path, None);
                    }
                } else if matches!(entry.kind, Kind::File | Kind::Link(false)) {
                    self.start_download(vec![entry]);
                }
            }
            None => {}
        }
    }

    // ----- input -----------------------------------------------------------

    pub fn key(&mut self, key: Key) -> Action {
        self.dirty = true;
        if self.dialog.is_some() {
            self.dialog_key(&key);
            return Action::None;
        }
        if self.help {
            self.help = false;
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')
            ) {
                return Action::None;
            }
        }
        let page = self.list_rows.max(2) as isize - 1;
        match key.code {
            _ if key.is_ctrl('n') => return Action::Close,
            KeyCode::Esc => return Action::Close,
            KeyCode::Tab => return Action::FocusTerminal,
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Home => self.move_by(isize::MIN / 2),
            KeyCode::End => self.move_by(isize::MAX / 2),
            KeyCode::Enter | KeyCode::Right => self.activate(),
            KeyCode::Left | KeyCode::Backspace => self.go_up(),
            KeyCode::Delete => self.start_delete(),
            KeyCode::F(2) => self.start_rename(),
            KeyCode::F(5) => self.refresh(),
            KeyCode::Char(c) if key.ctrl => match c {
                'r' => self.refresh(),
                'p' => self.move_by(-1),
                'u' => self.move_by(-page / 2),
                'd' => self.move_by(page / 2),
                _ => {}
            },
            KeyCode::Char(c) if !key.alt => return self.char_key(c),
            _ => {}
        }
        Action::None
    }

    fn char_key(&mut self, c: char) -> Action {
        match c {
            'q' => return Action::Close,
            'j' => self.move_by(1),
            'k' => self.move_by(-1),
            'g' => self.move_by(isize::MIN / 2),
            'G' => self.move_by(isize::MAX / 2),
            'l' if self.selected_entry().is_some_and(Entry::is_dir_like)
                || self.current_row() == Some(Row::Parent) =>
            {
                self.activate()
            }
            'h' => self.go_up(),
            ' ' => {
                self.toggle_mark();
                self.move_by(1);
            }
            'a' => self.toggle_all(),
            'n' => self.start_input(InputKind::NewFile, ""),
            'm' => self.start_input(InputKind::NewDir, ""),
            'u' => {
                let start = local_start_dir();
                self.start_input(InputKind::Upload, &start);
            }
            'd' => {
                let targets = self.targets();
                self.start_download(targets);
            }
            'x' | 'D' => self.start_delete(),
            'R' => self.start_rename(),
            'r' => self.refresh(),
            's' => self.start_size(),
            '.' => {
                let keep = self.selected_entry().map(|e| e.name.clone());
                self.show_hidden = !self.show_hidden;
                if !self.show_hidden {
                    // Never act on entries that can't be seen.
                    self.marked.retain(|name| !name.starts_with('.'));
                }
                self.selected = keep.and_then(|n| self.row_of(&n)).unwrap_or(0);
                self.clamp_selection();
                let state = if self.show_hidden { "shown" } else { "hidden" };
                self.say(Tone::Info, format!("hidden files {state}"));
            }
            '~' => self.list("", None),
            '/' => {
                let cwd = self.cwd.clone().unwrap_or_default();
                self.start_input(InputKind::Goto, &cwd);
            }
            'c' => self.cancel_job(),
            '?' => self.help = !self.help,
            '<' | '-' => return Action::Resize(-4),
            '>' | '+' => return Action::Resize(4),
            _ => {}
        }
        Action::None
    }

    fn toggle_mark(&mut self) {
        if let Some(name) = self.selected_entry().map(|e| e.name.clone())
            && !self.marked.remove(&name)
        {
            self.marked.insert(name);
        }
    }

    fn toggle_all(&mut self) {
        let visible: Vec<String> = self
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Entry(i) => Some(self.entries[i].name.clone()),
                Row::Parent => None,
            })
            .collect();
        if visible.iter().all(|n| self.marked.contains(n)) {
            self.marked.clear();
        } else {
            self.marked.extend(visible);
        }
    }

    /// Mouse event with panel-relative coordinates.
    pub fn mouse(&mut self, m: Mouse) -> Action {
        match m.kind {
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                if self.dialog.is_none() && !self.help {
                    let step = WHEEL_STEP as isize;
                    self.move_by(if m.kind == MouseKind::ScrollUp {
                        -step
                    } else {
                        step
                    });
                }
                return Action::None;
            }
            MouseKind::Press => {}
            _ => return Action::None,
        }
        self.dirty = true;
        // Later boxes are drawn on top of earlier ones.
        let hit = self
            .hits
            .iter()
            .rev()
            .find(|h| h.row == m.row && h.cols.contains(&m.col))
            .map(|h| h.hit.clone());
        if self.help && !matches!(hit, Some(Hit::Close)) {
            self.help = false;
            return Action::None;
        }
        let Some(hit) = hit else {
            return Action::None;
        };
        if self.dialog.is_some() && !matches!(hit, Hit::DialogOk | Hit::DialogCancel | Hit::Inert) {
            // Clicking outside a dialog leaves it open.
            return Action::None;
        }
        match hit {
            Hit::Row(i) => {
                let toggle = m.button == Button::Right || (m.button == Button::Left && m.ctrl);
                if toggle {
                    self.selected = i;
                    self.toggle_mark();
                    self.last_click = None;
                } else if m.button == Button::Left {
                    let double = self
                        .last_click
                        .is_some_and(|(at, row)| row == i && at.elapsed() < DOUBLE_CLICK);
                    self.selected = i;
                    if double {
                        self.last_click = None;
                        self.activate();
                    } else {
                        self.last_click = Some((Instant::now(), i));
                    }
                }
            }
            Hit::Crumb(path) => self.list(&path, None),
            Hit::Button(b) => match b {
                Btn::NewFile => self.start_input(InputKind::NewFile, ""),
                Btn::NewDir => self.start_input(InputKind::NewDir, ""),
                Btn::Upload => {
                    let start = local_start_dir();
                    self.start_input(InputKind::Upload, &start);
                }
                Btn::Download => {
                    let targets = self.targets();
                    self.start_download(targets);
                }
                Btn::Delete => self.start_delete(),
            },
            Hit::Close => return Action::Close,
            Hit::Help => self.help = !self.help,
            Hit::CancelJob => self.cancel_job(),
            Hit::DialogOk => self.dialog_submit(),
            Hit::DialogCancel => self.dialog_cancel(),
            Hit::Inert => {}
        }
        Action::None
    }

    /// Pasted text. Terminals paste the paths of files dropped on them, so
    /// a paste of existing local paths uploads them.
    pub fn paste(&mut self, text: &str) {
        self.dirty = true;
        let dropped = parse_dropped(text);
        match &mut self.dialog {
            Some(Dialog::Input { kind, edit, .. }) => {
                if matches!(kind, InputKind::Upload) && !dropped.is_empty() {
                    self.dialog = None;
                    self.start_upload(dropped);
                } else {
                    edit.insert(text.lines().next().unwrap_or("").trim_end());
                }
            }
            Some(Dialog::TypeToConfirm { edit, .. }) => {
                edit.insert(text.lines().next().unwrap_or("").trim())
            }
            Some(_) => {}
            None if !dropped.is_empty() => self.start_upload(dropped),
            None => self.say(
                Tone::Error,
                "drop local files here to upload them (pasted text is not a local path)",
            ),
        }
    }

    // ----- operations ------------------------------------------------------

    fn start_input(&mut self, kind: InputKind, initial: &str) {
        if self.cwd.is_none() && !matches!(kind, InputKind::Goto) {
            self.say(Tone::Error, "not connected yet");
            return;
        }
        let lines = match &kind {
            InputKind::NewFile => vec!["Name of the new file:".to_string()],
            InputKind::NewDir => vec!["Name of the new folder:".to_string()],
            InputKind::Rename { from } => vec![format!("New name for {from}:")],
            InputKind::Upload => vec!["Local file or folder (Tab completes):".to_string()],
            InputKind::Download { paths } => vec![format!(
                "Save {} in (Tab completes):",
                describe_names(paths.iter().map(|p| sftp::file_name(p)))
            )],
            InputKind::Goto => vec!["Remote folder:".to_string()],
        };
        self.dialog = Some(Dialog::Input {
            kind,
            lines,
            edit: LineEdit::new(initial),
            error: None,
        });
    }

    fn start_rename(&mut self) {
        if let Some(entry) = self.selected_entry() {
            let from = entry.name.clone();
            self.start_input(InputKind::Rename { from: from.clone() }, &from);
        }
    }

    fn start_size(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        if !entry.is_dir_like() {
            self.say(
                Tone::Info,
                format!("{} is {}", entry.name, human_size(entry.size)),
            );
            return;
        }
        let Some(path) = self.remote_path(&entry.name) else {
            return;
        };
        let id = self.id();
        self.size_scans.insert(id, path.clone());
        self.nav.send(Task::Scan {
            id,
            paths: vec![path],
            follow: true,
        });
        self.say(Tone::Info, format!("measuring {}…", entry.name));
    }

    fn start_download(&mut self, targets: Vec<Entry>) {
        let targets: Vec<Entry> = targets
            .into_iter()
            .filter(|e| e.kind != Kind::Other)
            .collect();
        if targets.is_empty() {
            self.say(Tone::Info, "select a file or folder to download");
            return;
        }
        let paths: Vec<String> = targets
            .iter()
            .filter_map(|e| self.remote_path(&e.name))
            .collect();
        let dir = display_local(&self.download_dir);
        self.start_input(InputKind::Download { paths }, &dir);
    }

    fn start_upload(&mut self, sources: Vec<PathBuf>) {
        let Some(cwd) = self.cwd.clone() else {
            self.say(Tone::Error, "not connected yet");
            return;
        };
        let existing: Vec<String> = sources
            .iter()
            .filter_map(|s| s.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| self.entries.iter().any(|e| &e.name == n))
            .collect();
        if !existing.is_empty() {
            self.dialog = Some(Dialog::Confirm {
                kind: ConfirmKind::OverwriteUpload { sources },
                title: "Replace?".to_string(),
                lines: vec![
                    format!(
                        "{} already in {}.",
                        describe_names(existing.iter().map(String::as_str)),
                        cwd
                    ),
                    "Replace files with the uploaded ones?".to_string(),
                ],
                ok: "Replace",
                danger: true,
                on_ok: false,
            });
            return;
        }
        self.queue_upload(sources, cwd);
    }

    fn queue_upload(&mut self, sources: Vec<PathBuf>, dir: String) {
        let id = self.id();
        let label = describe_names(
            sources
                .iter()
                .filter_map(|s| s.file_name().and_then(|n| n.to_str())),
        );
        self.queue.push(Job {
            id,
            kind: JobKind::Upload,
            label,
            dest: None,
            progress: Progress::default(),
            started: None,
        });
        self.jobs.send(Task::Upload { id, sources, dir });
        self.dirty = true;
    }

    fn queue_download(&mut self, paths: Vec<String>, dir: PathBuf) {
        let id = self.id();
        let label = describe_names(paths.iter().map(|p| sftp::file_name(p)));
        self.queue.push(Job {
            id,
            kind: JobKind::Download,
            label,
            dest: Some(display_local(&dir)),
            progress: Progress::default(),
            started: None,
        });
        self.jobs.send(Task::Download {
            id,
            sources: paths,
            dir,
        });
        self.dirty = true;
    }

    fn start_delete(&mut self) {
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let paths: Vec<String> = targets
            .iter()
            .filter_map(|e| self.remote_path(&e.name))
            .collect();
        // A single file or link: its size is already known.
        if let [one] = targets.as_slice()
            && one.kind != Kind::Dir
        {
            let summary = Summary {
                files: 1,
                dirs: 0,
                bytes: one.size,
            };
            self.confirm_delete(paths, Some(summary));
            return;
        }
        let id = self.id();
        // Deleting a link removes only the link, so it is not followed.
        self.nav.send(Task::Scan {
            id,
            paths: paths.clone(),
            follow: false,
        });
        self.dialog = Some(Dialog::Scanning { id, paths });
    }

    /// The first delete confirmation, once we know what would be removed.
    fn confirm_delete(&mut self, paths: Vec<String>, summary: Option<Summary>) {
        let entries: Vec<Option<&Entry>> = paths
            .iter()
            .map(|p| {
                let name = sftp::file_name(p);
                self.entries.iter().find(|e| e.name == name)
            })
            .collect();
        let mut lines = Vec::new();
        match (paths.as_slice(), entries.as_slice()) {
            ([path], [entry]) => {
                let name = sftp::file_name(path);
                let what = match entry.map(|e| e.kind) {
                    Some(Kind::Dir) => "folder",
                    Some(Kind::Link(_)) => "link",
                    _ => "file",
                };
                lines.push(format!("Delete {what} \"{name}\"?"));
                if what == "folder" {
                    lines.push("Everything inside it is deleted too.".to_string());
                }
                if what == "link" {
                    lines.push("Only the link is removed, not what it points to.".to_string());
                }
            }
            _ => {
                lines.push(format!("Delete these {} items?", paths.len()));
                lines.push(describe_names(paths.iter().map(|p| sftp::file_name(p))));
            }
        }
        if let Some(s) = summary {
            lines.push(summary_text(&s));
        }
        lines.push(format!("in {}", sftp::parent(&paths[0])));
        lines.push("This cannot be undone.".to_string());
        self.dialog = Some(Dialog::Confirm {
            kind: ConfirmKind::Delete { paths },
            title: "Delete".to_string(),
            lines,
            ok: "Delete",
            danger: true,
            on_ok: false,
        });
    }

    fn cancel_job(&mut self) {
        let Some(job) = self.queue.first() else {
            self.say(Tone::Info, "nothing to cancel");
            return;
        };
        let id = job.id;
        self.jobs.cancel(id);
        self.say(Tone::Info, "cancelling…");
    }

    // ----- dialogs ---------------------------------------------------------

    fn dialog_key(&mut self, key: &Key) {
        match &mut self.dialog {
            Some(Dialog::Input {
                kind, edit, error, ..
            }) => match key.code {
                KeyCode::Esc => self.dialog = None,
                KeyCode::Enter => self.dialog_submit(),
                KeyCode::Tab => {
                    if let Some(dirs_only) = kind.local_path() {
                        match complete_local(&edit.value(), dirs_only) {
                            Some(done) => *edit = LineEdit::new(&done),
                            None => *error = Some("no match".to_string()),
                        }
                    }
                }
                _ => {
                    if edit.key(key) {
                        *error = None;
                    }
                }
            },
            Some(Dialog::TypeToConfirm { edit, error, .. }) => match key.code {
                KeyCode::Esc => self.dialog = None,
                KeyCode::Enter => self.dialog_submit(),
                _ => {
                    if edit.key(key) {
                        *error = None;
                    }
                }
            },
            Some(Dialog::Confirm { on_ok, .. }) => match key.code {
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => self.dialog = None,
                KeyCode::Char('y') => self.dialog_submit(),
                KeyCode::Enter => {
                    if *on_ok {
                        self.dialog_submit();
                    } else {
                        self.dialog = None;
                    }
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    *on_ok = !*on_ok
                }
                KeyCode::Char('h') => *on_ok = false,
                KeyCode::Char('l') => *on_ok = true,
                _ => {}
            },
            Some(Dialog::Scanning { .. }) => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n')
                ) {
                    self.dialog_cancel();
                }
            }
            None => {}
        }
    }

    fn dialog_cancel(&mut self) {
        if let Some(Dialog::Scanning { id, .. }) = &self.dialog {
            self.nav.cancel(*id);
        }
        self.dialog = None;
    }

    fn dialog_submit(&mut self) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        match dialog {
            Dialog::Input {
                kind,
                lines,
                edit,
                error: _,
            } => {
                if let Err(e) = self.submit_input(&kind, &edit.value()) {
                    self.dialog = Some(Dialog::Input {
                        kind,
                        lines,
                        edit,
                        error: Some(e),
                    });
                }
            }
            Dialog::Confirm { kind, .. } => match kind {
                ConfirmKind::Delete { paths } => {
                    let (expected, prompt) = match paths.as_slice() {
                        [one] => {
                            let name = sftp::file_name(one).to_string();
                            let prompt = format!("Type the name \"{name}\" to delete it:");
                            (name, prompt)
                        }
                        many => (
                            "delete".to_string(),
                            format!("Type \"delete\" to delete {} items:", many.len()),
                        ),
                    };
                    self.dialog = Some(Dialog::TypeToConfirm {
                        paths,
                        expected,
                        lines: vec![prompt],
                        edit: LineEdit::default(),
                        error: None,
                    });
                }
                ConfirmKind::OverwriteUpload { sources } => {
                    if let Some(cwd) = self.cwd.clone() {
                        self.queue_upload(sources, cwd);
                    }
                }
                ConfirmKind::OverwriteDownload { paths, dir } => self.queue_download(paths, dir),
            },
            Dialog::TypeToConfirm {
                paths,
                expected,
                lines,
                edit,
                error: _,
            } => {
                if edit.value() == expected {
                    let id = self.id();
                    let label = describe_names(paths.iter().map(|p| sftp::file_name(p)));
                    self.queue.push(Job {
                        id,
                        kind: JobKind::Delete,
                        label,
                        dest: None,
                        progress: Progress::default(),
                        started: None,
                    });
                    self.marked.clear();
                    self.jobs.send(Task::Delete { id, paths });
                } else {
                    let error = if edit.value().is_empty() {
                        "type it exactly to confirm, or Esc to cancel"
                    } else {
                        "that does not match; nothing was deleted"
                    };
                    self.dialog = Some(Dialog::TypeToConfirm {
                        paths,
                        expected,
                        lines,
                        edit,
                        error: Some(error.to_string()),
                    });
                }
            }
            Dialog::Scanning { id, paths } => {
                // Nothing to submit until the scan is done.
                self.dialog = Some(Dialog::Scanning { id, paths });
            }
        }
    }

    fn submit_input(&mut self, kind: &InputKind, value: &str) -> Result<(), String> {
        let cwd = self.cwd.clone().unwrap_or_default();
        match kind {
            InputKind::NewFile | InputKind::NewDir => {
                let name = check_name(value)?;
                if self.entries.iter().any(|e| e.name == name) {
                    return Err(format!("{name} already exists"));
                }
                let task = if matches!(kind, InputKind::NewFile) {
                    Task::Touch { dir: cwd, name }
                } else {
                    Task::Mkdir { dir: cwd, name }
                };
                self.nav.send(task);
            }
            InputKind::Rename { from } => {
                let name = check_name(value)?;
                if &name == from {
                    return Ok(());
                }
                if self.entries.iter().any(|e| e.name == name) {
                    return Err(format!("{name} already exists"));
                }
                self.nav.send(Task::Rename {
                    dir: cwd,
                    from: from.clone(),
                    to: name,
                });
            }
            InputKind::Upload => {
                let value = value.trim();
                if value.is_empty() {
                    return Err("enter a path".to_string());
                }
                let path = expand_local(value);
                if !path.exists() {
                    return Err(format!("{} does not exist", display_local(&path)));
                }
                self.start_upload(vec![path]);
            }
            InputKind::Download { paths } => {
                let value = value.trim();
                let dir = expand_local(if value.is_empty() { "." } else { value });
                if !dir.is_dir() {
                    return Err(format!("{} is not a folder", display_local(&dir)));
                }
                let existing: Vec<&str> = paths
                    .iter()
                    .map(|p| sftp::file_name(p))
                    .filter(|n| dir.join(n).exists())
                    .collect();
                if existing.is_empty() {
                    self.queue_download(paths.clone(), dir);
                } else {
                    self.dialog = Some(Dialog::Confirm {
                        title: "Replace?".to_string(),
                        lines: vec![
                            format!(
                                "{} already in {}.",
                                describe_names(existing.iter().copied()),
                                display_local(&dir)
                            ),
                            "Replace the local copies?".to_string(),
                        ],
                        kind: ConfirmKind::OverwriteDownload {
                            paths: paths.clone(),
                            dir,
                        },
                        ok: "Replace",
                        danger: true,
                        on_ok: false,
                    });
                }
            }
            InputKind::Goto => {
                let value = value.trim();
                let path = if value.is_empty() || value == "~" {
                    String::new()
                } else if let Some(rest) = value.strip_prefix("~/") {
                    // The remote home is whatever "." resolves to.
                    format!("./{rest}")
                } else if value.starts_with('/') {
                    value.to_string()
                } else {
                    sftp::join(&cwd, value)
                };
                self.list(&path, None);
            }
        }
        Ok(())
    }

    // ----- drawing ---------------------------------------------------------

    /// Draw the panel. Returns the canvas and, when a prompt is open, where
    /// the cursor goes.
    pub fn render(&mut self, w: u16, h: u16, focused: bool) -> (Canvas, Option<(u16, u16)>) {
        self.dirty = false;
        self.hits.clear();
        let p = self.palette;
        let mut c = Canvas::new(w, h, Style::default());
        if w < 12 || h < 4 {
            c.text(0, 0, w, "SFTP", p.fg(p.accent).bold());
            return (c, None);
        }

        // Header: badge, server name, help and close buttons.
        let header_bg = if focused {
            p.frame
        } else {
            mix(p.frame, (0, 0, 0), 0.4)
        };
        let header = p.on(p.fg(p.text), header_bg);
        c.fill(0, 0, w, header);
        let badge = if focused {
            p.badge((20, 22, 30), p.accent).bold()
        } else {
            p.on(p.fg(p.accent).bold(), header_bg)
        };
        let mut col = c.text(0, 0, w, " SFTP ", badge);
        col += c.text(0, col, w - col, " ", header);
        let name_room = w.saturating_sub(col + 6);
        c.text(
            0,
            col,
            name_room,
            &truncate(&self.server, name_room as usize),
            header.bold(),
        );
        c.text(0, w - 5, 1, "?", header.bold());
        self.hits.push(HitBox {
            row: 0,
            cols: w - 6..w - 3,
            hit: Hit::Help,
        });
        c.text(0, w - 2, 1, "✕", header.bold());
        self.hits.push(HitBox {
            row: 0,
            cols: w - 3..w,
            hit: Hit::Close,
        });

        // Breadcrumb path.
        self.render_path(&mut c, 1, w);

        // Footer layout, from the bottom up.
        let mut bottom = h;
        bottom -= 1;
        let buttons_row = bottom;
        bottom -= 1;
        let status_row = bottom;
        let job_rows = if self.queue.is_empty() || h < 10 {
            0
        } else {
            2
        };
        bottom -= job_rows;
        let list_top = 2u16;
        let list_end = bottom.max(list_top);
        self.list_rows = (list_end - list_top) as usize;

        if self.help {
            self.render_help(&mut c, list_top, list_end, w);
        } else {
            self.render_list(&mut c, list_top, list_end, w, focused);
        }
        if job_rows > 0 {
            self.render_job(&mut c, list_end, w);
        }
        self.render_status(&mut c, status_row, w);
        self.render_buttons(&mut c, buttons_row, w);

        let cursor = self.render_dialog(&mut c, list_end, w);
        (c, cursor)
    }

    fn render_path(&mut self, c: &mut Canvas, row: u16, w: u16) {
        let p = self.palette;
        let Some(cwd) = self.cwd.clone() else {
            let text = if self.waiting_login {
                " waiting for login…"
            } else {
                " connecting…"
            };
            c.text(row, 0, w, text, p.fg(p.dim));
            return;
        };
        // (text, folder it opens) for each piece; separators open nothing.
        let mut pieces: Vec<(String, Option<String>)> =
            vec![("/".to_string(), Some("/".to_string()))];
        let mut acc = String::new();
        for (i, part) in cwd.split('/').filter(|s| !s.is_empty()).enumerate() {
            acc.push('/');
            acc.push_str(part);
            if i > 0 {
                pieces.push(("/".to_string(), None));
            }
            pieces.push((part.to_string(), Some(acc.clone())));
        }
        let last = pieces.len() - 1;
        // Drop pieces from the left until the rest fits behind an ellipsis.
        let room = w.saturating_sub(2) as usize;
        let mut start = 0;
        let mut total: usize = pieces.iter().map(|(t, _)| width(t)).sum();
        while total + usize::from(start > 0) > room && start < last {
            total -= width(&pieces[start].0);
            start += 1;
        }
        let mut col = 1u16;
        if start > 0 {
            col += c.text(row, col, 1, "…", p.fg(p.dim));
        }
        for (i, (text, target)) in pieces.iter().enumerate().skip(start) {
            if col >= w {
                break;
            }
            let style = if i == last {
                p.fg(p.text).bold()
            } else if target.is_none() {
                p.fg(p.frame)
            } else {
                p.fg(p.dim)
            };
            let room = (w - col) as usize;
            let used = c.text(row, col, w - col, &truncate(text, room), style);
            if let Some(target) = target
                && i != last
            {
                self.hits.push(HitBox {
                    row,
                    cols: col..col + used,
                    hit: Hit::Crumb(target.clone()),
                });
            }
            col += used;
        }
    }

    fn render_list(&mut self, c: &mut Canvas, top: u16, end: u16, w: u16, focused: bool) {
        let p = self.palette;
        let height = (end - top) as usize;
        if height == 0 {
            return;
        }
        let rows = self.rows();
        if self.cwd.is_none() {
            let text = match &self.list_error {
                Some(e) => e.clone(),
                None if self.waiting_login => "The panel opens once you are logged in.".into(),
                None => "Loading…".into(),
            };
            let style = if self.list_error.is_some() {
                p.fg(p.crit)
            } else {
                p.fg(p.dim)
            };
            for (i, line) in wrap(&text, (w - 2) as usize)
                .iter()
                .take(height)
                .enumerate()
            {
                c.text(top + i as u16, 1, w - 2, line, style);
            }
            return;
        }

        // Keep the selection on screen.
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
        if self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(height));

        let size_w: u16 = 6;
        let name_w = w.saturating_sub(size_w + 3);
        for (i, row) in rows.iter().enumerate().skip(self.scroll).take(height) {
            let y = top + (i - self.scroll) as u16;
            let selected = i == self.selected;
            let (name, base, size, size_style, marked) = match row {
                Row::Parent => (
                    "..".to_string(),
                    p.fg(p.dir).bold(),
                    String::new(),
                    p.fg(p.dim),
                    false,
                ),
                Row::Entry(idx) => {
                    let e = &self.entries[*idx];
                    let marked = self.marked.contains(&e.name);
                    let (suffix, style) = match e.kind {
                        Kind::Dir => ("/", p.fg(p.dir).bold()),
                        Kind::Link(true) => ("/", p.fg(p.link)),
                        Kind::Link(false) => ("@", p.fg(p.link)),
                        Kind::Other => ("", p.fg(p.warn)),
                        Kind::File if e.mode.is_some_and(|m| m & 0o111 != 0) => ("*", p.fg(p.exec)),
                        Kind::File => ("", p.fg(p.text)),
                    };
                    let (size, size_style) = match e.kind {
                        Kind::Dir | Kind::Link(true) => match self
                            .remote_path(&e.name)
                            .and_then(|path| self.dir_sizes.get(&path))
                        {
                            Some(s) => (human_size(s.bytes), p.fg(p.accent)),
                            None if e.kind == Kind::Dir => ("—".to_string(), p.fg(p.frame)),
                            None => ("→".to_string(), p.fg(p.frame)),
                        },
                        _ => (human_size(e.size), p.fg(p.dim)),
                    };
                    (
                        format!("{}{suffix}", e.name),
                        style,
                        size,
                        size_style,
                        marked,
                    )
                }
            };
            let fill = if selected {
                p.selection(Style::default(), focused)
            } else {
                Style::default()
            };
            c.fill(y, 0, w, fill);
            let apply = |s: Style| if selected { p.selection(s, focused) } else { s };
            if marked {
                c.text(y, 0, 1, "●", apply(p.fg(p.accent).bold()));
            }
            let mut name_style = base;
            if marked && !selected {
                name_style.bold = true;
            }
            c.text(
                y,
                1,
                name_w,
                &truncate(&name, name_w as usize),
                apply(name_style),
            );
            let size_text = format!("{size:>6}");
            c.text(y, w - size_w - 1, size_w, &size_text, apply(size_style));
            self.hits.push(HitBox {
                row: y,
                cols: 0..w,
                hit: Hit::Row(i),
            });
        }
        if rows.is_empty() || (rows.len() == 1 && rows[0] == Row::Parent) {
            let y = top + rows.len() as u16;
            if y < end {
                let hidden = self.entries.iter().filter(|e| e.is_hidden()).count();
                let text = if hidden > 0 && !self.show_hidden {
                    format!("only hidden files ({hidden}); press . to show")
                } else {
                    "empty folder".to_string()
                };
                c.text(y, 2, w - 2, &truncate(&text, (w - 3) as usize), p.fg(p.dim));
            }
        }
        // Scroll position, when the list does not fit: a thumb in the last
        // column (no track, it would double the border next to it).
        if rows.len() > height && height > 1 {
            let size = (height * height / rows.len()).max(1);
            let room = height - size;
            let max_scroll = rows.len() - height;
            let start = (self.scroll * room).div_ceil(max_scroll.max(1)).min(room);
            for y in start..start + size {
                let y = top + y as u16;
                let bg = c
                    .get(y, w - 1)
                    .map(|cell| cell.style.bg)
                    .unwrap_or_default();
                let mut style = p.fg(p.dim);
                style.bg = bg;
                c.text(y, w - 1, 1, "┃", style);
            }
        }
    }

    fn render_job(&mut self, c: &mut Canvas, row: u16, w: u16) {
        let p = self.palette;
        let Some(job) = self.queue.first() else {
            return;
        };
        let pr = &job.progress;
        let (icon, verb) = match job.kind {
            JobKind::Upload => ("↑", "Uploading"),
            JobKind::Download => ("↓", "Downloading"),
            JobKind::Delete => ("✕", "Deleting"),
        };
        let mut line = format!(" {icon} ");
        let name = if pr.current.is_empty() {
            &job.label
        } else {
            &pr.current
        };
        if job.started.is_none() {
            line.push_str(&format!("waiting: {}", job.label));
        } else if pr.total_files > 1 {
            line.push_str(&format!(
                "{}/{} {name}",
                (pr.files + 1).min(pr.total_files),
                pr.total_files
            ));
        } else {
            line.push_str(&format!("{verb} {name}"));
        }
        let queued = self.queue.len() - 1;
        let tail = if queued > 0 {
            format!(" +{queued}")
        } else {
            String::new()
        };
        let room = (w as usize).saturating_sub(width(&tail) + 1);
        c.text(row, 0, w, &truncate(&line, room), p.fg(p.text));
        if !tail.is_empty() {
            c.text(row, w - width(&tail) as u16 - 1, w, &tail, p.fg(p.dim));
        }

        // Second row: bar, percentage, speed and the cancel button.
        let y = row + 1;
        let cancel = " ✕";
        let right = if job.kind == JobKind::Delete {
            format!(" {} files", pr.files)
        } else {
            let pct = if pr.total_bytes > 0 {
                (pr.bytes as f64 / pr.total_bytes as f64 * 100.0).min(100.0)
            } else {
                0.0
            };
            let speed = job
                .started
                .map(|s| s.elapsed().as_secs_f64())
                .filter(|s| *s > 0.5)
                .map(|s| format!(" {}/s", human_size((pr.bytes as f64 / s) as u64)))
                .unwrap_or_default();
            format!(" {pct:>3.0}%{speed}")
        };
        let bar_w = (w as usize).saturating_sub(width(&right) + width(cancel) + 2);
        let frac = if job.kind == JobKind::Delete {
            None
        } else if pr.total_bytes > 0 {
            Some((pr.bytes as f64 / pr.total_bytes as f64).min(1.0))
        } else {
            Some(if pr.total_files > 0 && pr.files >= pr.total_files {
                1.0
            } else {
                0.0
            })
        };
        let mut col = 1;
        if let Some(frac) = frac {
            let eighths = (frac * bar_w as f64 * 8.0).round() as usize;
            let full = (eighths / 8).min(bar_w);
            const PARTS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
            let mut bar = "█".repeat(full);
            if full < bar_w {
                bar.push_str(PARTS[eighths % 8]);
            }
            let filled = width(&bar);
            col += c.text(y, col, w, &bar, p.fg(p.accent));
            col += c.text(y, col, w, &"░".repeat(bar_w - filled), p.fg(p.frame));
        } else {
            col += c.text(y, col, w, &"░".repeat(bar_w), p.fg(p.frame));
        }
        c.text(y, col, w, &right, p.fg(p.dim));
        let at = w - width(cancel) as u16;
        c.text(y, at, w, cancel, p.fg(p.crit).bold());
        self.hits.push(HitBox {
            row: y,
            cols: at.saturating_sub(1)..w,
            hit: Hit::CancelJob,
        });
    }

    fn render_status(&mut self, c: &mut Canvas, row: u16, w: u16) {
        let p = self.palette;
        let room = (w - 1) as usize;
        if let Some(m) = &self.message {
            c.text(row, 1, w - 1, &truncate(&m.text, room), p.tone(m.tone));
            return;
        }
        if self.cwd.is_none() {
            return;
        }
        if !self.marked.is_empty() {
            let bytes: u64 = self
                .entries
                .iter()
                .filter(|e| self.marked.contains(&e.name) && e.kind != Kind::Dir)
                .map(|e| e.size)
                .sum();
            let text = format!("{} marked · {}", self.marked.len(), human_size(bytes));
            c.text(row, 1, w - 1, &truncate(&text, room), p.fg(p.accent));
            return;
        }
        let text = match self.selected_entry() {
            Some(e) => {
                let mut parts = Vec::new();
                if let Some(mode) = e.mode {
                    parts.push(mode_string(mode));
                }
                if e.kind != Kind::Dir {
                    parts.push(human_size(e.size));
                }
                if let Some(t) = e.mtime {
                    parts.push(format_time(t));
                }
                parts.join("  ")
            }
            None => {
                let shown = self.rows().iter().filter(|r| **r != Row::Parent).count();
                let total: u64 = self
                    .entries
                    .iter()
                    .filter(|e| e.kind == Kind::File)
                    .map(|e| e.size)
                    .sum();
                format!("{shown} items · {}  (? for help)", human_size(total))
            }
        };
        c.text(row, 1, w - 1, &truncate(&text, room), p.fg(p.dim));
    }

    fn render_buttons(&mut self, c: &mut Canvas, row: u16, w: u16) {
        let p = self.palette;
        const SETS: [[&str; 5]; 3] = [
            ["+ File", "+ Folder", "↑ Upload", "↓ Download", "✕ Delete"],
            ["+File", "+Dir", "↑Up", "↓Get", "✕Del"],
            ["+F", "+D", "↑", "↓", "✕"],
        ];
        const BTNS: [Btn; 5] = [
            Btn::NewFile,
            Btn::NewDir,
            Btn::Upload,
            Btn::Download,
            Btn::Delete,
        ];
        let fits =
            |set: &[&str; 5]| set.iter().map(|l| width(l) + 2).sum::<usize>() + 4 <= w as usize;
        let set = SETS.iter().find(|s| fits(s)).unwrap_or(&SETS[2]);
        let mut col = 0u16;
        for (label, btn) in set.iter().zip(BTNS) {
            let text = format!(" {label} ");
            let tw = width(&text) as u16;
            if col + tw > w {
                break;
            }
            let fg = if btn == Btn::Delete { p.crit } else { p.text };
            let style = p.badge(fg, mix(p.frame, (0, 0, 0), 0.35));
            c.text(row, col, tw, &text, style);
            self.hits.push(HitBox {
                row,
                cols: col..col + tw,
                hit: Hit::Button(btn),
            });
            col += tw + 1;
        }
    }

    fn render_help(&mut self, c: &mut Canvas, top: u16, end: u16, w: u16) {
        let p = self.palette;
        const HELP: &[(&str, &str)] = &[
            ("↑↓ j k", "move"),
            ("Enter →", "open folder / download file"),
            ("← ⌫ h", "parent folder"),
            ("Space", "mark (a: mark all)"),
            ("n", "new file"),
            ("m", "new folder"),
            ("u", "upload (or drop files here)"),
            ("d", "download"),
            ("Del x", "delete (asks twice)"),
            ("R F2", "rename"),
            ("s", "folder size"),
            (".", "show hidden files"),
            ("r F5", "refresh"),
            ("/  ~", "go to path / home"),
            ("c", "cancel transfer"),
            ("< >", "narrower / wider"),
            ("Tab", "focus the shell"),
            ("Ctrl+N", "focus panel / close"),
            ("Esc q", "close panel"),
            (
                "mouse",
                "click, double-click, right-click marks, wheel, drag the border",
            ),
        ];
        let mut y = top;
        c.text(y, 1, w - 1, "Keys", p.fg(p.accent).bold());
        y += 1;
        let key_w = 9u16;
        for (keys, what) in HELP {
            if y >= end {
                break;
            }
            c.text(y, 1, key_w, keys, p.fg(p.text).bold());
            let room = w.saturating_sub(key_w + 2);
            c.text(
                y,
                key_w + 1,
                room,
                &truncate(what, room as usize),
                p.fg(p.dim),
            );
            y += 1;
        }
        for row in top..end {
            self.hits.push(HitBox {
                row,
                cols: 0..w,
                hit: Hit::Inert,
            });
        }
    }

    /// Draw the open dialog above row `bottom`; returns the cursor position
    /// of its text field.
    fn render_dialog(&mut self, c: &mut Canvas, bottom: u16, w: u16) -> Option<(u16, u16)> {
        let p = self.palette;
        let dialog = self.dialog.as_ref()?;
        let inner = w.saturating_sub(4) as usize;
        let mut lines: Vec<(String, Style)> = Vec::new();
        let mut field: Option<&LineEdit> = None;
        let mut error: Option<&str> = None;
        let (title, ok, danger, on_ok): (String, Option<&str>, bool, bool) = match dialog {
            Dialog::Input {
                kind,
                lines: text,
                edit,
                error: e,
            } => {
                for l in text {
                    lines.extend(wrap(l, inner).into_iter().map(|l| (l, p.fg(p.text))));
                }
                field = Some(edit);
                error = e.as_deref();
                (kind.title().to_string(), Some("OK"), false, true)
            }
            Dialog::Confirm {
                title,
                lines: text,
                ok,
                danger,
                on_ok,
                ..
            } => {
                for (i, l) in text.iter().enumerate() {
                    let style = if i == 0 {
                        p.fg(p.text).bold()
                    } else if l.starts_with("This cannot") {
                        p.fg(p.crit).bold()
                    } else {
                        p.fg(p.dim)
                    };
                    lines.extend(wrap(l, inner).into_iter().map(|l| (l, style)));
                }
                (title.clone(), Some(*ok), *danger, *on_ok)
            }
            Dialog::Scanning { paths, .. } => {
                let text = format!(
                    "Counting what deleting {} would remove…",
                    describe_names(paths.iter().map(|p| sftp::file_name(p)))
                );
                lines.extend(wrap(&text, inner).into_iter().map(|l| (l, p.fg(p.dim))));
                ("Delete".to_string(), None, true, false)
            }
            Dialog::TypeToConfirm {
                lines: text,
                edit,
                error: e,
                ..
            } => {
                for l in text {
                    lines.extend(wrap(l, inner).into_iter().map(|l| (l, p.fg(p.text).bold())));
                }
                lines.push(("This cannot be undone.".to_string(), p.fg(p.crit).bold()));
                field = Some(edit);
                error = e.as_deref();
                ("Confirm delete".to_string(), Some("Delete"), true, true)
            }
        };
        let errors: Vec<String> = error.map(|e| wrap(e, inner)).unwrap_or_default();
        let height = 2 + lines.len() + usize::from(field.is_some()) + errors.len() + 1;
        let height = height.min(bottom as usize) as u16;
        let top = bottom.saturating_sub(height);
        let border = p.fg(if danger { p.crit } else { p.accent });
        let body = Style::default();

        for y in top..bottom {
            c.fill(y, 0, w, body);
            self.hits.push(HitBox {
                row: y,
                cols: 0..w,
                hit: Hit::Inert,
            });
        }
        // Frame.
        let title = format!(" {title} ");
        let mut top_line = String::from("╭─");
        top_line.push_str(&title);
        let used = width(&top_line);
        top_line.push_str(&"─".repeat((w as usize).saturating_sub(used + 1)));
        top_line.push('╮');
        c.text(top, 0, w, &top_line, border);
        c.text(top, 2, width(&title) as u16, &title, border.bold());
        let mut bottom_line = String::from("╰");
        bottom_line.push_str(&"─".repeat((w as usize).saturating_sub(2)));
        bottom_line.push('╯');
        c.text(bottom - 1, 0, w, &bottom_line, border);
        for y in top + 1..bottom - 1 {
            c.text(y, 0, 1, "│", border);
            c.text(y, w - 1, 1, "│", border);
        }

        let mut y = top + 1;
        let last_body_row = bottom.saturating_sub(2);
        for (text, style) in &lines {
            if y >= last_body_row {
                break;
            }
            c.text(y, 2, inner as u16, text, *style);
            y += 1;
        }
        let mut cursor = None;
        if let Some(edit) = field
            && y < last_body_row
        {
            let field_w = inner.saturating_sub(2);
            let (shown, cur) = edit.view(field_w);
            let field_style = p.on(p.fg(p.text), mix(p.frame, (0, 0, 0), 0.55));
            c.text(y, 2, 2, "› ", p.fg(p.accent).bold());
            c.fill(y, 4, field_w as u16, field_style);
            c.text(y, 4, field_w as u16, &shown, field_style);
            cursor = Some((y, 4 + cur as u16));
            y += 1;
        }
        for e in &errors {
            if y >= last_body_row {
                break;
            }
            c.text(y, 2, inner as u16, e, p.fg(p.crit));
            y += 1;
        }
        // Buttons on the last row inside the frame.
        let by = last_body_row;
        let cancel = " Cancel ";
        let mut col = w.saturating_sub(2);
        if let Some(ok) = ok {
            let ok_text = format!(" {ok} ");
            let ow = width(&ok_text) as u16;
            col = col.saturating_sub(ow);
            let style = if on_ok {
                p.badge((20, 22, 30), if danger { p.crit } else { p.accent })
                    .bold()
            } else {
                p.on(
                    p.fg(if danger { p.crit } else { p.accent }),
                    mix(p.frame, (0, 0, 0), 0.35),
                )
            };
            c.text(by, col, ow, &ok_text, style);
            self.hits.push(HitBox {
                row: by,
                cols: col..col + ow,
                hit: Hit::DialogOk,
            });
            col = col.saturating_sub(1);
        }
        let cw = width(cancel) as u16;
        col = col.saturating_sub(cw);
        let cancel_focused = !on_ok && ok.is_some() && field.is_none();
        let style = if cancel_focused || ok.is_none() {
            p.badge((20, 22, 30), p.text).bold()
        } else {
            p.on(p.fg(p.text), mix(p.frame, (0, 0, 0), 0.35))
        };
        c.text(by, col, cw, cancel, style);
        self.hits.push(HitBox {
            row: by,
            cols: col..col + cw,
            hit: Hit::DialogCancel,
        });
        cursor
    }
}

/// Folders first, then by name (case-insensitive).
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir_like()
            .cmp(&a.is_dir_like())
            .then_with(|| natural_cmp(&a.name, &b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Case-insensitive order that compares runs of digits as numbers, so
/// `f2` comes before `f10`.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut digits = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        it.next();
                    }
                    digits
                };
                let (da, db) = (take(&mut a), take(&mut b));
                let (ta, tb) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                let order = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                a.next();
                b.next();
            }
        }
    }
}

fn check_name(value: &str) -> Result<String, String> {
    let name = value.trim();
    if name.is_empty() {
        return Err("enter a name".to_string());
    }
    if name.contains('/') {
        return Err("a name cannot contain /".to_string());
    }
    if name == "." || name == ".." {
        return Err("that name is reserved".to_string());
    }
    Ok(name.to_string())
}

/// "a.txt", "a.txt and b.txt", "a.txt, b.txt and 3 more".
fn describe_names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    match names.as_slice() {
        [] => "nothing".to_string(),
        [one] => one.to_string(),
        [a, b] => format!("{a} and {b}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

fn summary_text(s: &Summary) -> String {
    let plural = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut parts = vec![plural(s.files, "file", "files")];
    if s.dirs > 0 {
        parts.push(plural(s.dirs, "folder", "folders"));
    }
    format!("{}, {} in total", parts.join(" in "), human_size(s.bytes))
}

/// Word-wrap `text` to `cols` columns.
fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if width(&candidate) <= cols {
            line = candidate;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        // Hard-break words longer than a line.
        let mut rest = word.to_string();
        while width(&rest) > cols {
            let mut head = String::new();
            let mut used = 0;
            let mut chars = rest.chars().peekable();
            while let Some(&ch) = chars.peek() {
                let cw = width(&ch.to_string());
                if used + cw > cols {
                    break;
                }
                head.push(ch);
                used += cw;
                chars.next();
            }
            if head.is_empty() {
                break;
            }
            lines.push(head);
            rest = chars.collect();
        }
        line = rest;
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// `drwxr-xr-x` from a mode.
fn mode_string(mode: u32) -> String {
    let kind = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o020000 => 'c',
        0o060000 => 'b',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// Local date and time of a Unix timestamp: `2026-10-31 14:05`.
fn format_time(secs: u32) -> String {
    let t = libc::time_t::from(secs);
    // SAFETY: localtime_r only writes into the struct we pass.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !ok {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

fn home() -> Option<PathBuf> {
    store::home_dir().ok()
}

/// Where downloads go: `download_dir` from the config, else ~/Downloads,
/// else the directory lssh was started in.
fn default_download_dir(configured: &str) -> PathBuf {
    if !configured.trim().is_empty() {
        return expand_local(configured.trim());
    }
    if let Some(dir) = home().map(|h| h.join("Downloads"))
        && dir.is_dir()
    {
        return dir;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The upload prompt starts in the directory lssh was started in.
fn local_start_dir() -> String {
    let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut text = display_local(&dir);
    if !text.ends_with('/') {
        text.push('/');
    }
    text
}

/// `~`, `~/x` and relative paths made absolute.
fn expand_local(input: &str) -> PathBuf {
    let path = if input == "~" {
        home().unwrap_or_default()
    } else if let Some(rest) = input.strip_prefix("~/") {
        home().unwrap_or_default().join(rest)
    } else {
        PathBuf::from(input)
    };
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

/// A local path with the home directory shown as `~`.
fn display_local(path: &Path) -> String {
    if let Some(home) = home()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

/// Tab completion of a local path. Returns the completed text, or `None`
/// when nothing matches.
fn complete_local(input: &str, dirs_only: bool) -> Option<String> {
    let (dir_part, prefix) = match input.rfind('/') {
        Some(i) => (&input[..=i], &input[i + 1..]),
        None => ("", input),
    };
    let dir = expand_local(if dir_part.is_empty() { "." } else { dir_part });
    let mut matches: Vec<(String, bool)> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            let is_dir = std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir());
            (!dirs_only || is_dir).then_some((name, is_dir))
        })
        .collect();
    matches.sort();
    match matches.as_slice() {
        [] => None,
        [(name, is_dir)] => Some(format!(
            "{dir_part}{name}{}",
            if *is_dir { "/" } else { "" }
        )),
        many => {
            let first: Vec<char> = many[0].0.chars().collect();
            let mut common = first.len();
            for (name, _) in &many[1..] {
                common = common.min(
                    first
                        .iter()
                        .zip(name.chars())
                        .take_while(|(a, b)| **a == *b)
                        .count(),
                );
            }
            let shared: String = first[..common].iter().collect();
            Some(format!("{dir_part}{shared}"))
        }
    }
}

/// Local paths from a paste. Terminals paste dropped files as shell-quoted
/// paths, `file://` URIs, or one path per line. Empty unless every piece
/// names an existing file or folder.
pub fn parse_dropped(text: &str) -> Vec<PathBuf> {
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if text.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let pieces: Vec<String> = if lines.len() > 1 {
        lines
            .iter()
            .flat_map(|l| {
                let words = shell_words(l);
                if words.len() == 1 {
                    words
                } else {
                    vec![l.to_string()]
                }
            })
            .collect()
    } else {
        shell_words(text)
    };
    let paths: Option<Vec<PathBuf>> = pieces.iter().map(|p| dropped_path(p)).collect();
    if let Some(paths) = paths.filter(|p| !p.is_empty()) {
        return paths;
    }
    // A single unquoted path with spaces in it.
    dropped_path(text).into_iter().collect()
}

fn dropped_path(piece: &str) -> Option<PathBuf> {
    let piece = piece.trim();
    let path = if let Some(rest) = piece.strip_prefix("file://") {
        // file:///path or file://localhost/path
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        let host_end = rest.find('/')?;
        let path = &rest[host_end..];
        PathBuf::from(percent_decode(path))
    } else if piece.starts_with('/') || piece.starts_with("~/") {
        expand_local(piece)
    } else {
        return None;
    };
    path.exists().then_some(path)
}

/// Decode `%xx` escapes (as in `file://` URLs).
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Split like a POSIX shell: whitespace separates words; quotes and
/// backslashes escape.
fn shell_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                            word.push(chars.next().unwrap_or('\\'));
                        }
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_unquote() {
        assert_eq!(
            shell_words("'/tmp/a b' /tmp/c\\ d \"/x/\\\"q\\\"\""),
            vec!["/tmp/a b", "/tmp/c d", "/x/\"q\""]
        );
        assert!(shell_words("   ").is_empty());
    }

    #[test]
    fn dropped_paths_in_every_format() {
        let dir = std::env::temp_dir().join(format!("lssh-drop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a b.txt");
        let b = dir.join("c.txt");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        let (sa, sb) = (a.display().to_string(), b.display().to_string());

        // GNOME Terminal style: quoted, trailing space.
        assert_eq!(
            parse_dropped(&format!("'{sa}' '{sb}' ")),
            vec![a.clone(), b.clone()]
        );
        // Backslash-escaped (kitty, WezTerm).
        assert_eq!(parse_dropped(&sa.replace(' ', "\\ ")), vec![a.clone()]);
        // URI list.
        let uri = format!("file://{}", sa.replace(' ', "%20"));
        assert_eq!(
            parse_dropped(&format!("{uri}\r\nfile://{sb}\r\n")),
            vec![a.clone(), b.clone()]
        );
        // A bare path with a space (Alacritty).
        assert_eq!(parse_dropped(&sa), vec![a.clone()]);
        // Not paths.
        assert!(parse_dropped("hello world").is_empty());
        assert!(parse_dropped(&format!("{sb} /does/not/exist")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_edit_basics() {
        let mut e = LineEdit::new("abc");
        e.key(&Key::new(KeyCode::Left));
        e.key(&Key::new(KeyCode::Char('X')));
        assert_eq!(e.value(), "abXc");
        e.key(&Key::new(KeyCode::Backspace));
        assert_eq!(e.value(), "abc");
        let ctrl_w = Key {
            ctrl: true,
            ..Key::new(KeyCode::Char('w'))
        };
        let mut e = LineEdit::new("~/dir/file");
        e.key(&ctrl_w);
        assert_eq!(e.value(), "~/dir/");
        let (shown, cur) = LineEdit::new("0123456789").view(5);
        assert_eq!((shown.as_str(), cur), ("6789", 4));
    }

    #[test]
    fn names_and_summaries() {
        assert!(check_name("a/b").is_err());
        assert!(check_name("..").is_err());
        assert_eq!(check_name(" x ").unwrap(), "x");
        assert_eq!(
            describe_names(["a", "b", "c", "d"].into_iter()),
            "a, b and 2 more"
        );
        let s = Summary {
            files: 3,
            dirs: 1,
            bytes: 2048,
        };
        assert_eq!(summary_text(&s), "3 files in 1 folder, 2.0K in total");
        assert_eq!(mode_string(0o040755), "drwxr-xr-x");
        assert_eq!(mode_string(0o100600), "-rw-------");
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), vec![""]);
    }

    #[test]
    fn sorting_puts_folders_first() {
        let e = |name: &str, kind| Entry {
            name: name.into(),
            kind,
            size: 0,
            mode: None,
            mtime: None,
        };
        let mut list = vec![
            e("b.txt", Kind::File),
            e("Zeta", Kind::Dir),
            e("alpha", Kind::Dir),
            e("A.txt", Kind::File),
            e("link", Kind::Link(true)),
        ];
        sort_entries(&mut list);
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "link", "Zeta", "A.txt", "b.txt"]);
        let mut list = vec![
            e("f10", Kind::File),
            e("f2", Kind::File),
            e("F1", Kind::File),
            e("f02b", Kind::File),
        ];
        sort_entries(&mut list);
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["F1", "f2", "f02b", "f10"]);
    }
}
