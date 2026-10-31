//! A grid of styled cells and a renderer that only redraws what changed.
//!
//! While the file panel is open LSSH draws the whole shell area itself: the
//! panel on the left and the remote screen (from the `vt100` emulator) on
//! the right. Frames are diffed against the previous one row by row, so a
//! keypress costs a few bytes instead of a full repaint.

use unicode_width::UnicodeWidthChar;

pub use vt100::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Style {
    pub fn fg(color: Color) -> Style {
        Style {
            fg: color,
            ..Style::default()
        }
    }

    pub fn on(mut self, bg: Color) -> Style {
        self.bg = bg;
        self
    }

    pub fn bold(mut self) -> Style {
        self.bold = true;
        self
    }

    pub fn dim(mut self) -> Style {
        self.dim = true;
        self
    }

    pub fn inverse(mut self) -> Style {
        self.inverse = true;
        self
    }

    pub fn underline(mut self) -> Style {
        self.underline = true;
        self
    }
}

/// An RGB colour.
pub fn rgb(c: (u8, u8, u8)) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    /// Combining characters that follow `ch`, if any.
    pub extra: Option<Box<str>>,
    pub style: Style,
    /// 1, 2 for the first half of a wide character, 0 for its second half.
    pub width: u8,
}

impl Cell {
    pub fn blank(style: Style) -> Cell {
        Cell {
            ch: ' ',
            extra: None,
            style,
            width: 1,
        }
    }

    /// Convert a cell of the remote screen.
    pub fn from_vt(cell: &vt100::Cell) -> Cell {
        let style = Style {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        };
        if cell.is_wide_continuation() {
            return Cell {
                ch: ' ',
                extra: None,
                style,
                width: 0,
            };
        }
        let text = cell.contents();
        let mut chars = text.chars();
        let ch = chars.next().unwrap_or(' ');
        // Control characters must never reach the terminal raw.
        let ch = if ch.is_control() { ' ' } else { ch };
        let rest = chars.as_str();
        Cell {
            ch,
            extra: (!rest.is_empty()).then(|| rest.into()),
            style,
            width: if cell.is_wide() { 2 } else { 1 },
        }
    }
}

/// A rectangle of cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<Cell>,
}

impl Canvas {
    pub fn new(width: u16, height: u16, style: Style) -> Canvas {
        Canvas {
            width,
            height,
            cells: vec![Cell::blank(style); width as usize * height as usize],
        }
    }

    fn index(&self, row: u16, col: u16) -> Option<usize> {
        (row < self.height && col < self.width)
            .then(|| row as usize * self.width as usize + col as usize)
    }

    pub fn get(&self, row: u16, col: u16) -> Option<&Cell> {
        self.index(row, col).map(|i| &self.cells[i])
    }

    pub fn set(&mut self, row: u16, col: u16, cell: Cell) {
        if let Some(i) = self.index(row, col) {
            self.cells[i] = cell;
        }
    }

    pub fn row(&self, row: u16) -> &[Cell] {
        let start = row as usize * self.width as usize;
        &self.cells[start..start + self.width as usize]
    }

    /// Paint `width` blank cells.
    pub fn fill(&mut self, row: u16, col: u16, width: u16, style: Style) {
        for c in col..col.saturating_add(width).min(self.width) {
            self.set(row, c, Cell::blank(style));
        }
    }

    /// Write `text` from `col`, using at most `max` columns. Returns the
    /// columns used. A wide character that does not fit is not drawn.
    pub fn text(&mut self, row: u16, col: u16, max: u16, text: &str, style: Style) -> u16 {
        let limit = col.saturating_add(max).min(self.width);
        let mut at = col;
        for ch in text.chars() {
            let ch = if ch.is_control() { '?' } else { ch };
            let w = ch.width().unwrap_or(0) as u16;
            if w == 0 {
                // Combining mark: attach to the previous cell.
                if at > col
                    && let Some(i) = self.index(row, at - 1)
                {
                    let cell = &mut self.cells[i];
                    let mut extra = cell.extra.take().map(String::from).unwrap_or_default();
                    extra.push(ch);
                    cell.extra = Some(extra.into());
                }
                continue;
            }
            if at + w > limit {
                break;
            }
            self.set(
                row,
                at,
                Cell {
                    ch,
                    extra: None,
                    style,
                    width: w as u8,
                },
            );
            if w == 2 {
                self.set(
                    row,
                    at + 1,
                    Cell {
                        ch: ' ',
                        extra: None,
                        style,
                        width: 0,
                    },
                );
            }
            at += w;
        }
        at - col
    }

    /// Copy `src` into this canvas with its top-left corner at (`row`, `col`).
    pub fn blit(&mut self, src: &Canvas, row: u16, col: u16) {
        for r in 0..src.height {
            for c in 0..src.width {
                if let Some(cell) = src.get(r, c) {
                    self.set(row + r, col + c, cell.clone());
                }
            }
        }
    }
}

/// Display width of a string.
pub fn width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Cut `text` to `max` columns, ending in `…` when something was cut.
pub fn truncate(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

fn push_color(out: &mut String, color: Color, fg: bool) {
    match color {
        Color::Default => {}
        Color::Idx(n) if n < 8 => {
            out.push_str(&format!(";{}", if fg { 30 } else { 40 } + u16::from(n)));
        }
        Color::Idx(n) if n < 16 => {
            out.push_str(&format!(
                ";{}",
                if fg { 90 } else { 100 } + u16::from(n - 8)
            ));
        }
        Color::Idx(n) => out.push_str(&format!(";{};5;{n}", if fg { 38 } else { 48 })),
        Color::Rgb(r, g, b) => {
            out.push_str(&format!(";{};2;{r};{g};{b}", if fg { 38 } else { 48 }))
        }
    }
}

/// SGR sequence that sets exactly `style`.
pub fn sgr(style: &Style) -> String {
    let mut out = String::from("\x1b[0");
    if style.bold {
        out.push_str(";1");
    }
    if style.dim {
        out.push_str(";2");
    }
    if style.italic {
        out.push_str(";3");
    }
    if style.underline {
        out.push_str(";4");
    }
    if style.inverse {
        out.push_str(";7");
    }
    push_color(&mut out, style.fg, true);
    push_color(&mut out, style.bg, false);
    out.push('m');
    out
}

/// Escape sequences that turn the screen showing `prev` into `next`, both
/// drawn with their top-left corner at the terminal's top-left. With no
/// `prev` every row is drawn. Autowrap must be off.
pub fn diff(prev: Option<&Canvas>, next: &Canvas, out: &mut String) {
    let prev = prev.filter(|p| p.width == next.width && p.height == next.height);
    let mut current: Option<Style> = None;
    for row in 0..next.height {
        let new = next.row(row);
        let (mut start, mut end) = match prev {
            Some(prev) => {
                let old = prev.row(row);
                let Some(first) = (0..new.len()).find(|&i| new[i] != old[i]) else {
                    continue;
                };
                let last = (0..new.len())
                    .rev()
                    .find(|&i| new[i] != old[i])
                    .unwrap_or(first);
                // Start on the first half of a wide character, old or new.
                let mut first = first;
                while first > 0 && (new[first].width == 0 || old[first].width == 0) {
                    first -= 1;
                }
                (first, last)
            }
            None => (0, new.len().saturating_sub(1)),
        };
        if new.is_empty() {
            continue;
        }
        // Finish a wide character that starts at the end of the span.
        if new[end].width == 2 && end + 1 < new.len() {
            end += 1;
        }
        // A continuation cell at the start has no first half to draw.
        while start <= end && new[start].width == 0 {
            start += 1;
        }
        if start > end {
            continue;
        }
        out.push_str(&format!("\x1b[{};{}H", row + 1, start + 1));
        let mut col = start;
        while col <= end {
            let cell = &new[col];
            if cell.width == 0 {
                // Orphaned second half (its first half was not drawable).
                if current != Some(cell.style) {
                    out.push_str(&sgr(&cell.style));
                    current = Some(cell.style);
                }
                out.push(' ');
                col += 1;
                continue;
            }
            if current != Some(cell.style) {
                out.push_str(&sgr(&cell.style));
                current = Some(cell.style);
            }
            out.push(cell.ch);
            if let Some(extra) = &cell.extra {
                out.push_str(extra);
            }
            col += cell.width.max(1) as usize;
        }
    }
    if current.is_some() {
        out.push_str("\x1b[0m");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(rows: &[&str]) -> Canvas {
        let width = rows.iter().map(|r| width(r)).max().unwrap_or(0) as u16;
        let mut c = Canvas::new(width, rows.len() as u16, Style::default());
        for (i, row) in rows.iter().enumerate() {
            c.text(i as u16, 0, width, row, Style::default());
        }
        c
    }

    #[test]
    fn full_draw_positions_every_row() {
        let mut out = String::new();
        diff(None, &canvas(&["ab", "cd"]), &mut out);
        assert!(out.contains("\x1b[1;1H\x1b[0mab"));
        assert!(out.contains("\x1b[2;1Hcd"));
        assert!(out.ends_with("\x1b[0m"));
    }

    #[test]
    fn diff_only_touches_changed_cells() {
        let old = canvas(&["hello", "world"]);
        let new = canvas(&["hello", "wOrld"]);
        let mut out = String::new();
        diff(Some(&old), &new, &mut out);
        assert_eq!(out, "\x1b[2;2H\x1b[0mO\x1b[0m");

        let mut out = String::new();
        diff(Some(&new), &new, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn wide_characters_are_redrawn_whole() {
        let old = canvas(&["a中b"]);
        let new = canvas(&["a文b"]);
        let mut out = String::new();
        diff(Some(&old), &new, &mut out);
        assert_eq!(out, "\x1b[1;2H\x1b[0m文\x1b[0m");
        assert_eq!(new.get(0, 2).unwrap().width, 0);
    }

    #[test]
    fn text_respects_width() {
        let mut c = Canvas::new(4, 1, Style::default());
        assert_eq!(c.text(0, 0, 4, "ab中文", Style::default()), 4);
        assert_eq!(c.get(0, 3).unwrap().width, 0);
        let mut c = Canvas::new(3, 1, Style::default());
        assert_eq!(c.text(0, 0, 3, "ab中", Style::default()), 2);
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("中文中文", 5), "中文…");
    }

    #[test]
    fn sgr_codes() {
        let s = Style::fg(Color::Idx(1)).on(Color::Rgb(1, 2, 3)).bold();
        assert_eq!(sgr(&s), "\x1b[0;1;31;48;2;1;2;3m");
        assert_eq!(sgr(&Style::fg(Color::Idx(9))), "\x1b[0;91m");
        assert_eq!(sgr(&Style::fg(Color::Idx(200))), "\x1b[0;38;5;200m");
    }
}
