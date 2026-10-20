//! Shared bits for the full-screen views (`lssh` picker and `lssh top`).

use std::io;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::{cursor, execute, terminal};

/// Raw mode on the alternate screen; the terminal is restored on drop, even
/// on early return or panic.
pub struct FullScreen;

impl FullScreen {
    pub fn enter() -> Result<FullScreen> {
        terminal::enable_raw_mode().context("failed to enable raw mode")?;
        let screen = FullScreen;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)
            .context("failed to switch to the alternate screen")?;
        Ok(screen)
    }
}

impl Drop for FullScreen {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

/// Esc, Ctrl-C or q (when `q_quits`).
pub fn is_quit(key: &KeyEvent, q_quits: bool) -> bool {
    match key.code {
        KeyCode::Esc => true,
        KeyCode::Char('c') | KeyCode::Char('d') => key.modifiers.contains(KeyModifiers::CONTROL),
        KeyCode::Char('q') => q_quits && key.modifiers.is_empty(),
        _ => false,
    }
}

/// Cut `text` to `width` characters, ending in `…` when shortened.
pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut cut: String = text.chars().take(width - 1).collect();
    cut.push('…');
    cut
}

/// `text` truncated and padded to exactly `width` characters.
pub fn fit(text: &str, width: usize) -> String {
    let cut = truncate(text, width);
    let pad = width - cut.chars().count();
    format!("{cut}{}", " ".repeat(pad))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_and_fits() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(truncate("hello", 0), "");
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("abcdef", 4), "abc…");
    }

    #[test]
    fn quit_keys() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        assert!(is_quit(&key(KeyCode::Esc, KeyModifiers::NONE), false));
        assert!(is_quit(
            &key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            false
        ));
        assert!(!is_quit(
            &key(KeyCode::Char('q'), KeyModifiers::NONE),
            false
        ));
        assert!(is_quit(&key(KeyCode::Char('q'), KeyModifiers::NONE), true));
    }
}
