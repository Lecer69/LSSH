//! `lssh top`: the stats bars of several servers on one screen.
//!
//! Each server gets a background ssh connection (a multiplexing master
//! that only authenticates with a saved password or key) and a worker
//! thread that polls its stats over that connection.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{cursor, queue};

use crate::askpass::Askpass;
use crate::config::Config;
use crate::out;
use crate::stats::{self, Collector, Stats};
use crate::store::{self, Server};
use crate::tui::{self, FullScreen};
use crate::ui::{self, BarInfo, History, Theme};

enum Update {
    Stats(usize, Stats),
    Failed(usize, String),
}

enum State {
    Connecting,
    Up,
    Failed(String),
}

struct Host {
    server: Server,
    address: String,
    state: State,
    stats: Option<Stats>,
    history: History,
}

pub fn run(servers: Vec<Server>, config: Config) -> Result<()> {
    if !out::interactive() {
        bail!("lssh top needs a terminal");
    }
    let dir = store::runtime_dir().join(format!("lssh-top-{}", std::process::id()));
    store::create_private_dir(&dir)?;

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let interval = Duration::from_millis(config.update_interval_ms);
    let workers: Vec<_> = servers
        .iter()
        .enumerate()
        .map(|(index, server)| {
            let worker = Worker {
                index,
                server: server.clone(),
                socket: dir.join(index.to_string()),
                interval,
                stop: Arc::clone(&stop),
                updates: tx.clone(),
            };
            thread::spawn(move || worker.run())
        })
        .collect();
    drop(tx);

    let mut hosts: Vec<Host> = servers
        .into_iter()
        .map(|server| Host {
            address: server.address(),
            server,
            state: State::Connecting,
            stats: None,
            history: History::default(),
        })
        .collect();

    let result = (|| -> Result<()> {
        let _screen = FullScreen::enter()?;
        let theme = Theme::from_config(&config);
        let mut scroll = 0usize;
        let mut dirty = true;
        loop {
            if dirty {
                draw(&hosts, scroll, &config, &theme)?;
                dirty = false;
            }
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if tui::is_quit(&key, true) {
                            break;
                        }
                        match key.code {
                            KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                            KeyCode::Down | KeyCode::Char('j') => {
                                scroll = (scroll + 1).min(hosts.len().saturating_sub(1));
                            }
                            _ => {}
                        }
                        dirty = true;
                    }
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
            }
            while let Ok(update) = rx.try_recv() {
                match update {
                    Update::Stats(i, stats) => {
                        hosts[i].history.push(&stats);
                        hosts[i].stats = Some(stats);
                        hosts[i].state = State::Up;
                    }
                    Update::Failed(i, reason) => hosts[i].state = State::Failed(reason),
                }
                dirty = true;
            }
        }
        Ok(())
    })();

    stop.store(true, Ordering::Relaxed);
    for worker in workers {
        let _ = worker.join();
    }
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn draw(hosts: &[Host], scroll: usize, config: &Config, theme: &Theme) -> io::Result<()> {
    let (cols, rows) = terminal::size()?;
    let width = cols as usize;
    let mut out = io::stdout();
    queue!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;

    let up = hosts
        .iter()
        .filter(|h| matches!(h.state, State::Up))
        .count();
    let title = format!(
        " lssh top · {up}/{} up · every {}",
        hosts.len(),
        human_interval(config.update_interval_ms)
    );
    queue!(out, Print(ui::render_text(&title, true, width, theme)))?;
    let keys = "↑↓ scroll · q quit ";
    if width > title.chars().count() + keys.len() + 2 {
        queue!(
            out,
            cursor::MoveTo((width - keys.chars().count()) as u16, 0),
            Print(ui::render_text(keys, false, width, theme))
        )?;
    }

    // Each server takes two lines plus a blank one.
    let mut row = 2u16;
    for host in hosts.iter().skip(scroll) {
        if row + 2 > rows {
            let rest = format!(" … more below ({} servers)", hosts.len());
            queue!(
                out,
                cursor::MoveTo(0, rows.saturating_sub(1)),
                Print(ui::render_text(&rest, false, width, theme))
            )?;
            break;
        }
        let info = BarInfo {
            name: &host.server.name,
            address: &host.address,
        };
        let lines = match &host.state {
            State::Up => ui::render_bar(
                &info,
                host.stats.as_ref(),
                &host.history,
                width,
                2,
                config,
                theme,
            ),
            State::Connecting => vec![ui::render_status_line(
                &info,
                "connecting…",
                false,
                width,
                config,
                theme,
            )],
            State::Failed(reason) => vec![ui::render_status_line(
                &info, reason, true, width, config, theme,
            )],
        };
        for (i, line) in lines.iter().enumerate() {
            queue!(out, cursor::MoveTo(0, row + i as u16), Print(line))?;
        }
        row += 3;
    }
    out.flush()
}

fn human_interval(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Keeps one server's connection open and reports its stats.
struct Worker {
    index: usize,
    server: Server,
    socket: PathBuf,
    interval: Duration,
    stop: Arc<AtomicBool>,
    updates: Sender<Update>,
}

impl Worker {
    fn run(self) {
        let failed = |reason: String| {
            let _ = self.updates.send(Update::Failed(self.index, reason));
        };
        // Prompts can't be answered behind the dashboard: saved passwords
        // and keys only.
        let askpass = match Askpass::new(&self.server, false) {
            Ok(askpass) => askpass,
            Err(e) => return failed(format!("{e:#}")),
        };
        let mut master = match self.spawn_master(&askpass) {
            Ok(master) => master,
            Err(e) => return failed(format!("failed to launch ssh: {e}")),
        };

        let mut collector = Collector::new();
        while !self.stop.load(Ordering::Relaxed) {
            if let Ok(Some(status)) = master.try_wait() {
                let mut stderr = String::new();
                if let Some(mut pipe) = master.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                return failed(explain(&self.server, &stderr, &status.to_string()));
            }
            let mut wait = Duration::from_millis(200);
            if self.socket.exists() {
                if let Ok(raw) = stats::collect(&self.server.target(), &self.socket) {
                    let snapshot = stats::parse(&raw, &mut collector);
                    if self
                        .updates
                        .send(Update::Stats(self.index, snapshot))
                        .is_err()
                    {
                        break;
                    }
                }
                wait = self.interval;
            }
            self.sleep(wait);
        }
        let _ = master.kill();
        let _ = master.wait();
    }

    fn spawn_master(&self, askpass: &Askpass) -> io::Result<Child> {
        let mut command = Command::new("ssh");
        askpass.apply(&mut command);
        command
            .arg("-N")
            .args(["-o", "ControlMaster=yes"])
            .arg("-o")
            .arg(format!("ControlPath={}", self.socket.display()))
            .args([
                "-o",
                "ControlPersist=no",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "LogLevel=ERROR",
            ])
            .args(self.server.ssh_options())
            .arg(self.server.target())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
    }

    /// Sleep for `total`, waking early when asked to stop.
    fn sleep(&self, total: Duration) {
        let step = Duration::from_millis(50);
        let mut slept = Duration::ZERO;
        while slept < total && !self.stop.load(Ordering::Relaxed) {
            thread::sleep(step);
            slept += step;
        }
    }
}

/// Turn ssh's last words into what to do about them.
fn explain(server: &Server, stderr: &str, status: &str) -> String {
    let name = &server.name;
    if stderr.contains("Permission denied") {
        format!("✖ no saved password or key · connect once with: lssh {name}")
    } else if stderr.contains("Host key verification failed") {
        format!("✖ host key not trusted yet · connect once with: lssh {name}")
    } else {
        let last = stderr
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.trim_start_matches("ssh: ").to_string());
        format!(
            "✖ {}",
            last.unwrap_or_else(|| format!("ssh exited ({status})"))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_common_failures() {
        let server = Server::new("web", "root@10.0.0.1").unwrap();
        assert!(
            explain(
                &server,
                "root@10.0.0.1: Permission denied (publickey,password).",
                ""
            )
            .contains("lssh web")
        );
        assert!(explain(&server, "Host key verification failed.\n", "").contains("host key"));
        assert_eq!(
            explain(
                &server,
                "ssh: connect to host 10.0.0.1 port 22: Connection refused\n",
                ""
            ),
            "✖ connect to host 10.0.0.1 port 22: Connection refused"
        );
        assert_eq!(
            explain(&server, "", "exit status: 1"),
            "✖ ssh exited (exit status: 1)"
        );
    }

    #[test]
    fn intervals() {
        assert_eq!(human_interval(1000), "1s");
        assert_eq!(human_interval(1500), "1.5s");
    }
}
