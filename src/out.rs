//! Styled terminal messages, so every command speaks with the same voice.

use std::fmt::{self, Display};
use std::io::{self, ErrorKind, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};

use colored::Colorize;

use crate::store::Server;

/// Environment variable that turns on raw mode, like `--raw`.
pub const RAW_VAR: &str = "LSSH_RAW";

static RAW: AtomicBool = AtomicBool::new(false);

/// Turn colours off when stdout is not a terminal (e.g. `lssh ls | grep`).
/// `LSSH_RAW` (anything but empty or `0`) turns on raw mode.
pub fn init() {
    if !std::io::stdout().is_terminal() {
        colored::control::set_override(false);
    }
    if std::env::var(RAW_VAR).is_ok_and(|v| !v.is_empty() && v != "0") {
        set_raw();
    }
}

/// Raw mode: plain output for scripts. No colours, symbols, stats bar,
/// file panel or progress bars, and nothing is asked on the terminal.
pub fn set_raw() {
    RAW.store(true, Ordering::Relaxed);
    colored::control::set_override(false);
}

pub fn raw() -> bool {
    RAW.load(Ordering::Relaxed)
}

pub fn success(msg: impl Display) {
    if raw() {
        println!("{msg}");
    } else {
        println!("{} {msg}", "✔".green().bold());
    }
}

/// Something lssh is about to do. Not shown in raw mode.
pub fn step(msg: impl Display) {
    if !raw() {
        println!("{} {msg}", "→".cyan().bold());
    }
}

pub fn info(msg: impl Display) {
    if raw() {
        println!("{msg}");
    } else {
        println!("{} {msg}", "ℹ".cyan().bold());
    }
}

/// A dimmed follow-up line under a message, e.g. what to run next.
/// Not shown in raw mode.
pub fn hint(msg: impl Display) {
    if !raw() {
        println!("  {}", msg.to_string().dimmed());
    }
}

pub fn warn(msg: impl Display) {
    if raw() {
        eprintln!("warning: {msg}");
    } else {
        eprintln!("{} {msg}", "⚠".yellow().bold());
    }
}

/// Print an error. The first line is the error itself; any further lines
/// are hints and are shown dimmed underneath.
pub fn error(msg: &str) {
    let mut lines = msg.lines();
    eprintln!(
        "{} {}",
        "error:".red().bold(),
        lines.next().unwrap_or_default()
    );
    for line in lines {
        eprintln!("  {}", line.dimmed());
    }
}

/// A server's name, as highlighted in messages.
pub fn name(server: &Server) -> String {
    server.name.cyan().bold().to_string()
}

/// A server's `user@host[:port]`, as highlighted in messages.
pub fn target(server: &Server) -> String {
    let address = server.address();
    let (_, host_part) = address.split_once('@').unwrap_or(("", &address));
    let (host, port) = match server.port {
        Some(port) => (
            host_part.trim_end_matches(&format!(":{port}")).to_string(),
            format!(":{port}").dimmed().to_string(),
        ),
        None => (host_part.to_string(), String::new()),
    };
    format!(
        "{}{}{}{port}",
        server.user.yellow(),
        "@".dimmed(),
        host.blue()
    )
}

/// Whether both stdin and stdout are a terminal, so a full-screen UI works.
/// Never in raw mode.
pub fn interactive() -> bool {
    !raw() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// `5m ago`, `3h ago`, `12d ago`... for a Unix timestamp; `never` for none.
pub fn ago(when: Option<u64>, now: u64) -> String {
    let Some(when) = when else {
        return "never".to_string();
    };
    let secs = now.saturating_sub(when);
    match secs {
        0..60 => "just now".to_string(),
        60..3_600 => format!("{}m ago", secs / 60),
        3_600..86_400 => format!("{}h ago", secs / 3_600),
        _ if secs < 60 * 86_400 => format!("{}d ago", secs / 86_400),
        _ => format!("{}mo ago", secs / (30 * 86_400)),
    }
}

/// Why a connection failed, in a few words: `connection refused`, ...
pub fn connect_error(e: &io::Error) -> String {
    match e.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => "timed out".to_string(),
        ErrorKind::ConnectionRefused => "connection refused".to_string(),
        ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable => "unreachable".to_string(),
        _ => e.to_string(),
    }
}

/// Exit with this status without printing anything more (the reason is
/// already on screen, e.g. a remote command's own output).
#[derive(Debug)]
pub struct Exit(pub u8);

impl Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit status {}", self.0)
    }
}

impl std::error::Error for Exit {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_is_human() {
        let now = 1_000_000_000;
        assert_eq!(ago(None, now), "never");
        assert_eq!(ago(Some(now - 5), now), "just now");
        assert_eq!(ago(Some(now - 300), now), "5m ago");
        assert_eq!(ago(Some(now - 7_200), now), "2h ago");
        assert_eq!(ago(Some(now - 3 * 86_400), now), "3d ago");
        assert_eq!(ago(Some(now - 90 * 86_400), now), "3mo ago");
        assert_eq!(ago(Some(now + 50), now), "just now", "clock skew");
    }

    #[test]
    fn target_includes_port() {
        colored::control::set_override(false);
        let mut server = Server::new("web", "root@10.0.0.1:2222").unwrap();
        assert_eq!(target(&server), "root@10.0.0.1:2222");
        server.host = "::1".into();
        assert_eq!(target(&server), "root@[::1]:2222");
        server.port = None;
        assert_eq!(target(&server), "root@::1");
    }
}
