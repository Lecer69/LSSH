//! Styled terminal messages, so every command speaks with the same voice.

use std::fmt::Display;
use std::io::IsTerminal;

use colored::Colorize;

use crate::store::Server;

/// Turn colours off when stdout is not a terminal (e.g. `lssh ls | grep`).
pub fn init() {
    if !std::io::stdout().is_terminal() {
        colored::control::set_override(false);
    }
}

pub fn success(msg: impl Display) {
    println!("{} {msg}", "✔".green().bold());
}

/// Something lssh is about to do.
pub fn step(msg: impl Display) {
    println!("{} {msg}", "→".cyan().bold());
}

pub fn info(msg: impl Display) {
    println!("{} {msg}", "ℹ".cyan().bold());
}

/// A dimmed follow-up line under a message, e.g. what to run next.
pub fn hint(msg: impl Display) {
    println!("  {}", msg.to_string().dimmed());
}

pub fn warn(msg: impl Display) {
    eprintln!("{} {msg}", "⚠".yellow().bold());
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

/// A server's `user@host`, as highlighted in messages.
pub fn target(server: &Server) -> String {
    format!(
        "{}{}{}",
        server.user.yellow(),
        "@".dimmed(),
        server.host.blue()
    )
}
