//! The interactive server picker shown by plain `lssh`.

use std::io::{self, Write};

use anyhow::Result;
use colored::Colorize;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{cursor, queue};

use crate::out;
use crate::store::{self, Server};
use crate::tui::{self, FullScreen};

/// Let the user choose a server; `None` when they cancel.
pub fn pick(servers: &[&Server]) -> Result<Option<String>> {
    let mut picker = Picker::new(servers);
    let _screen = FullScreen::enter()?;
    let mut stdout = io::stdout();
    loop {
        picker.draw(&mut stdout)?;
        if let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            match picker.handle(&key) {
                Action::Stay => {}
                Action::Quit => return Ok(None),
                Action::Pick(name) => return Ok(Some(name)),
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Action {
    Stay,
    Quit,
    Pick(String),
}

struct Picker<'a> {
    servers: &'a [&'a Server],
    query: String,
    /// Indices into `servers` that match the query, best first.
    matches: Vec<usize>,
    /// Position in `matches`.
    selected: usize,
    /// First visible position in `matches`.
    offset: usize,
}

impl<'a> Picker<'a> {
    fn new(servers: &'a [&'a Server]) -> Self {
        let mut picker = Picker {
            servers,
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            offset: 0,
        };
        picker.filter();
        picker
    }

    fn filter(&mut self) {
        let mut scored: Vec<(i64, usize)> = self
            .servers
            .iter()
            .enumerate()
            .filter_map(|(i, server)| score(&self.query, server).map(|s| (s, i)))
            .collect();
        // Stable, so equal scores keep the most-recent-first order.
        scored.sort_by_key(|&(s, _)| std::cmp::Reverse(s));
        self.matches = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
        self.offset = 0;
    }

    fn handle(&mut self, key: &KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if tui::is_quit(key, false) {
            return Action::Quit;
        }
        match key.code {
            KeyCode::Enter => {
                if let Some(&i) = self.matches.get(self.selected) {
                    return Action::Pick(self.servers[i].name.clone());
                }
            }
            KeyCode::Up | KeyCode::BackTab => self.move_by(-1),
            KeyCode::Down | KeyCode::Tab => self.move_by(1),
            KeyCode::Char('p' | 'k') if ctrl => self.move_by(-1),
            KeyCode::Char('n' | 'j') if ctrl => self.move_by(1),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.filter();
            }
            KeyCode::Char('w') if ctrl => {
                let keep = self.query.trim_end().rfind(' ').map_or(0, |i| i + 1);
                self.query.truncate(keep);
                self.filter();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.filter();
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.filter();
            }
            _ => {}
        }
        Action::Stay
    }

    fn move_by(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    fn draw(&mut self, out: &mut impl Write) -> io::Result<()> {
        let (cols, rows) = terminal::size()?;
        let width = cols as usize;
        let visible = (rows as usize).saturating_sub(4).max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + visible {
            self.offset = self.selected + 1 - visible;
        }

        queue!(
            out,
            cursor::Hide,
            cursor::MoveTo(0, 0),
            Clear(ClearType::All)
        )?;

        // Prompt line, with the match count on the right.
        let count = format!("{}/{}", self.matches.len(), self.servers.len());
        let prompt = tui::truncate(&self.query, width.saturating_sub(count.len() + 12));
        let used = 9 + prompt.chars().count();
        queue!(
            out,
            Print(format!(
                "  {} {} {prompt}{}{}",
                "lssh".cyan().bold(),
                "❯".cyan(),
                " ".repeat(width.saturating_sub(used + count.len() + 2)),
                count.dimmed()
            ))
        )?;

        let now = store::now();
        let name_width = self
            .servers
            .iter()
            .map(|s| s.name.chars().count())
            .max()
            .unwrap_or(0)
            .min(24);
        let address_width = self
            .servers
            .iter()
            .map(|s| s.address().chars().count())
            .max()
            .unwrap_or(0)
            .min(width.saturating_sub(name_width + 6));
        let show_ago = width >= name_width + address_width + 20;

        for (row, &index) in self
            .matches
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(visible)
        {
            let server = self.servers[index];
            let name = tui::fit(&server.name, name_width);
            let address = tui::fit(&server.address(), address_width);
            let ago = if show_ago {
                out::ago(server.last_used, now)
            } else {
                String::new()
            };
            let line = if row == self.selected {
                format!(
                    "{} {}   {}   {}",
                    "▌".cyan().bold(),
                    name.cyan().bold(),
                    address,
                    ago.dimmed()
                )
            } else {
                format!("  {}   {}   {}", name, address.dimmed(), ago.dimmed())
            };
            let screen_row = (row - self.offset + 2) as u16;
            queue!(out, cursor::MoveTo(0, screen_row), Print(line))?;
        }
        if self.matches.is_empty() {
            queue!(
                out,
                cursor::MoveTo(2, 2),
                Print("no matching server".dimmed())
            )?;
        }

        let help = "↑↓ move · enter connect · esc quit";
        queue!(
            out,
            cursor::MoveTo(0, rows.saturating_sub(1)),
            Print(format!(
                "  {}",
                tui::truncate(help, width.saturating_sub(2)).dimmed()
            )),
            cursor::MoveTo((9 + prompt.chars().count()).min(width) as u16, 0),
            cursor::Show
        )?;
        out.flush()
    }
}

/// Rank `server` against `query`: a fuzzy match on the name scores higher
/// than one that also needs the address. `None` means no match.
fn score(query: &str, server: &Server) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let by_name = fuzzy(query, &server.name).map(|s| s + 100);
    let by_address = fuzzy(query, &format!("{} {}", server.name, server.address()));
    by_name.max(by_address)
}

/// Case-insensitive subsequence match. Matches at word starts and runs of
/// consecutive characters score higher; gaps cost points.
fn fuzzy(query: &str, text: &str) -> Option<i64> {
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0;
    let mut from = 0;
    let mut previous: Option<usize> = None;
    for wanted in query.to_lowercase().chars() {
        let pos = from + text.get(from..)?.iter().position(|&c| c == wanted)?;
        score += 10;
        if pos == 0 || !text[pos - 1].is_alphanumeric() {
            score += 8;
        }
        if previous.is_some_and(|p| p + 1 == pos) {
            score += 12;
        }
        score -= (pos - from) as i64;
        previous = Some(pos);
        from = pos + 1;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn servers() -> Vec<Server> {
        vec![
            Server::new("web", "root@11.12.13.14").unwrap(),
            Server::new("db", "admin@db.example.com").unwrap(),
            Server::new("webmail", "root@10.0.0.9").unwrap(),
        ]
    }

    fn names(picker: &Picker) -> Vec<String> {
        picker
            .matches
            .iter()
            .map(|&i| picker.servers[i].name.clone())
            .collect()
    }

    fn press(picker: &mut Picker, code: KeyCode) -> Action {
        picker.handle(&KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn fuzzy_matching() {
        assert!(fuzzy("wb", "web").is_some());
        assert!(fuzzy("bw", "web").is_none());
        assert!(fuzzy("WEB", "web").is_some());
        assert!(fuzzy("web", "web").unwrap() > fuzzy("web", "w_e_b").unwrap());
        assert!(fuzzy("", "anything").is_some());
        assert!(fuzzy("xyz", "").is_none());
    }

    #[test]
    fn filters_and_ranks() {
        let servers = servers();
        let refs: Vec<&Server> = servers.iter().collect();
        let mut picker = Picker::new(&refs);
        assert_eq!(names(&picker), ["web", "db", "webmail"]);

        for c in "web".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        assert_eq!(names(&picker), ["web", "webmail"]);

        // Addresses match too.
        press(&mut picker, KeyCode::Char('u'));
        picker.handle(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(picker.query, "");
        for c in "example".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        assert_eq!(names(&picker), ["db"]);
    }

    #[test]
    fn navigation_and_picking() {
        let servers = servers();
        let refs: Vec<&Server> = servers.iter().collect();
        let mut picker = Picker::new(&refs);
        press(&mut picker, KeyCode::Down);
        press(&mut picker, KeyCode::Down);
        press(&mut picker, KeyCode::Down);
        assert_eq!(picker.selected, 2, "stops at the last entry");
        press(&mut picker, KeyCode::Up);
        assert_eq!(
            press(&mut picker, KeyCode::Enter),
            Action::Pick("db".into())
        );
        assert_eq!(press(&mut picker, KeyCode::Esc), Action::Quit);

        for c in "zzz".chars() {
            press(&mut picker, KeyCode::Char(c));
        }
        assert_eq!(press(&mut picker, KeyCode::Enter), Action::Stay);
        press(&mut picker, KeyCode::Down);
        assert_eq!(picker.selected, 0);
    }
}
