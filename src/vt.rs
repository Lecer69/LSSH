//! A tiny VT parser sitting on the pty -> terminal stream.
//!
//! The remote side believes the screen is only `pty_rows` tall, but the real
//! terminal is taller and the bottom lines belong to the stats bar. This
//! filter keeps the two views consistent:
//!
//! * scroll regions (`CSI t;b r`) are clamped to the shell area, so a remote
//!   `CSI r` ("whole screen") can never make the bar scroll away;
//! * terminal resets re-establish our scroll region;
//! * erases that wipe the bar and alt-screen switches request a repaint;
//! * it knows whether the stream is between escape sequences / UTF-8
//!   characters, which is the only safe point to inject a bar repaint.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// Inside a UTF-8 character, with this many continuation bytes left.
    Utf8(u8),
    /// After ESC (held back until we know what follows).
    Esc,
    /// After ESC + intermediate byte(s), e.g. `ESC ( B`.
    EscInter,
    /// Buffering a CSI sequence so it can be inspected or rewritten.
    Csi,
    /// An over-long CSI sequence being passed through untouched.
    CsiPass,
    /// OSC / DCS / APC / PM / SOS string, ended by BEL or ST.
    Str,
}

/// What the caller should do after writing filtered output.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Events {
    /// The bar was (possibly) erased and should be drawn again.
    pub repaint: bool,
    /// The main screen came back from the alt screen; it may hold stale bar
    /// remnants from a resize that happened in the meantime.
    pub fixup: bool,
}

const MAX_CSI: usize = 64;

#[derive(Debug, Clone)]
pub struct Filter {
    state: State,
    csi: Vec<u8>,
    pty_rows: u16,
    autowrap: bool,
    alt_screen: bool,
}

impl Filter {
    pub fn new(pty_rows: u16) -> Self {
        Filter {
            state: State::Ground,
            csi: Vec::new(),
            pty_rows: pty_rows.max(1),
            autowrap: true,
            alt_screen: false,
        }
    }

    pub fn set_pty_rows(&mut self, rows: u16) {
        self.pty_rows = rows.max(1);
    }

    /// True when it is safe to inject our own escape sequences.
    pub fn at_ground(&self) -> bool {
        self.state == State::Ground
    }

    /// Whether the remote currently has autowrap (DECAWM) enabled.
    pub fn autowrap(&self) -> bool {
        self.autowrap
    }

    /// Whether the remote is currently on the alternate screen.
    pub fn alt_screen(&self) -> bool {
        self.alt_screen
    }

    /// Filter `input`, appending what should reach the terminal to `out`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Events {
        let mut ev = Events::default();
        for &b in input {
            self.step(b, out, &mut ev);
        }
        ev
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>, ev: &mut Events) {
        match self.state {
            State::Ground => self.ground(b, out),
            State::Utf8(left) => {
                if (0x80..=0xBF).contains(&b) {
                    out.push(b);
                    self.state = if left <= 1 {
                        State::Ground
                    } else {
                        State::Utf8(left - 1)
                    };
                } else {
                    // Malformed: drop back to ground and handle the byte.
                    self.state = State::Ground;
                    self.step(b, out, ev);
                }
            }
            State::Esc => match b {
                b'[' => {
                    self.csi.clear();
                    self.state = State::Csi;
                }
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    out.extend_from_slice(&[0x1b, b]);
                    self.state = State::Str;
                }
                b'c' => {
                    // RIS: full reset drops our scroll region and clears.
                    out.extend_from_slice(b"\x1bc");
                    out.extend_from_slice(self.region().as_bytes());
                    ev.repaint = true;
                    self.autowrap = true;
                    self.alt_screen = false;
                    self.state = State::Ground;
                }
                0x1b => out.push(0x1b),
                0x20..=0x2F => {
                    out.extend_from_slice(&[0x1b, b]);
                    self.state = State::EscInter;
                }
                _ => {
                    out.extend_from_slice(&[0x1b, b]);
                    self.state = State::Ground;
                }
            },
            State::EscInter => {
                out.push(b);
                if !(0x20..=0x2F).contains(&b) {
                    self.state = State::Ground;
                }
            }
            State::Csi => match b {
                0x40..=0x7E => {
                    self.finish_csi(b, out, ev);
                    self.state = State::Ground;
                }
                0x1b => {
                    self.flush_csi(out);
                    self.state = State::Esc;
                }
                0x18 | 0x1a => {
                    self.flush_csi(out);
                    out.push(b);
                    self.state = State::Ground;
                }
                // C0 controls are executed mid-sequence by the terminal.
                0x00..=0x1F => out.push(b),
                _ => {
                    self.csi.push(b);
                    if self.csi.len() > MAX_CSI {
                        self.flush_csi(out);
                        self.state = State::CsiPass;
                    }
                }
            },
            State::CsiPass => {
                out.push(b);
                if (0x40..=0x7E).contains(&b) || b == 0x18 || b == 0x1a {
                    self.state = State::Ground;
                }
            }
            State::Str => match b {
                0x07 | 0x18 | 0x1a => {
                    out.push(b);
                    self.state = State::Ground;
                }
                // Either ST (`ESC \`) or the start of a new sequence; the
                // Esc state handles both.
                0x1b => self.state = State::Esc,
                _ => out.push(b),
            },
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<u8>) {
        match b {
            0x1b => self.state = State::Esc,
            0xC0..=0xDF => {
                out.push(b);
                self.state = State::Utf8(1);
            }
            0xE0..=0xEF => {
                out.push(b);
                self.state = State::Utf8(2);
            }
            0xF0..=0xF7 => {
                out.push(b);
                self.state = State::Utf8(3);
            }
            _ => out.push(b),
        }
    }

    fn flush_csi(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[");
        out.extend_from_slice(&self.csi);
        self.csi.clear();
    }

    fn region(&self) -> String {
        format!("\x1b[1;{}r", self.pty_rows)
    }

    fn finish_csi(&mut self, fin: u8, out: &mut Vec<u8>, ev: &mut Events) {
        let csi = std::mem::take(&mut self.csi);
        let private = csi.first().copied().filter(|c| b"<=>?".contains(c));
        let has_inter = csi.iter().any(|c| (0x20..=0x2F).contains(c));
        let params_raw = if private.is_some() {
            &csi[1..]
        } else {
            &csi[..]
        };
        let params: Vec<u16> = std::str::from_utf8(params_raw)
            .unwrap_or("")
            .split(';')
            .map(|p| p.parse().unwrap_or(0))
            .collect();

        match (fin, private, has_inter) {
            // DECSTBM: keep the region inside the shell area.
            (b'r', None, false) => {
                let top = params.first().copied().unwrap_or(0).max(1);
                let mut bottom = params.get(1).copied().unwrap_or(0);
                if bottom == 0 || bottom > self.pty_rows {
                    bottom = self.pty_rows;
                }
                out.extend_from_slice(format!("\x1b[{top};{bottom}r").as_bytes());
                return;
            }
            // DECSTR (`CSI ! p`): soft reset clears the scroll region.
            (b'p', None, true) if csi.ends_with(b"!") => {
                out.extend_from_slice(b"\x1b[");
                out.extend_from_slice(&csi);
                out.push(fin);
                self.autowrap = true;
                out.extend_from_slice(format!("\x1b7{}\x1b8", self.region()).as_bytes());
                return;
            }
            // ED: everything but "erase above" reaches the bar rows.
            (b'J', None | Some(b'?'), false) if params.first().copied().unwrap_or(0) != 1 => {
                ev.repaint = true;
            }
            (b'h' | b'l', Some(b'?'), false) => {
                let set = fin == b'h';
                for p in &params {
                    match p {
                        7 => self.autowrap = set,
                        47 | 1047 | 1049 => {
                            ev.repaint = true;
                            if !set && self.alt_screen {
                                ev.fixup = true;
                            }
                            self.alt_screen = set;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        out.extend_from_slice(b"\x1b[");
        out.extend_from_slice(&csi);
        out.push(fin);
    }
}

/// Remove the first cursor position report (`ESC [ row ; col R`) from `buf`
/// and return it as 1-based `(row, col)`.
pub fn take_cursor_report(buf: &mut Vec<u8>) -> Option<(u16, u16)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' {
            let rest = &buf[i + 2..];
            let end = rest
                .iter()
                .position(|c| !(c.is_ascii_digit() || *c == b';'));
            if let Some(end) = end
                && rest[end] == b'R'
                && let Ok(text) = std::str::from_utf8(&rest[..end])
                && let Some((r, c)) = text.split_once(';')
                && let (Ok(r), Ok(c)) = (r.parse::<u16>(), c.parse::<u16>())
            {
                buf.drain(i..i + 2 + end + 1);
                return Some((r, c));
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(f: &mut Filter, input: &[u8]) -> (Vec<u8>, Events) {
        let mut out = Vec::new();
        let ev = f.feed(input, &mut out);
        (out, ev)
    }

    #[test]
    fn plain_text_passes_through() {
        let mut f = Filter::new(20);
        let (out, ev) = run(&mut f, "hello ☃\r\n\x1b[1;31mred\x1b[0m".as_bytes());
        assert_eq!(out, "hello ☃\r\n\x1b[1;31mred\x1b[0m".as_bytes());
        assert_eq!(ev, Events::default());
        assert!(f.at_ground());
    }

    #[test]
    fn full_screen_region_is_clamped() {
        let mut f = Filter::new(20);
        assert_eq!(run(&mut f, b"\x1b[r").0, b"\x1b[1;20r");
        assert_eq!(run(&mut f, b"\x1b[;r").0, b"\x1b[1;20r");
        assert_eq!(run(&mut f, b"\x1b[5;40r").0, b"\x1b[5;20r");
        assert_eq!(run(&mut f, b"\x1b[2;10r").0, b"\x1b[2;10r");
        // XTRESTORE (`CSI ? r`) is not a scroll region.
        assert_eq!(run(&mut f, b"\x1b[?1r").0, b"\x1b[?1r");
    }

    #[test]
    fn sequences_split_across_reads() {
        let mut f = Filter::new(10);
        let (out, _) = run(&mut f, b"ab\x1b");
        assert_eq!(out, b"ab");
        assert!(!f.at_ground());
        let (out, _) = run(&mut f, b"[");
        assert!(out.is_empty());
        let (out, _) = run(&mut f, b"r");
        assert_eq!(out, b"\x1b[1;10r");
        assert!(f.at_ground());

        let snowman = "☃".as_bytes();
        let (out, _) = run(&mut f, &snowman[..1]);
        assert_eq!(out, &snowman[..1]);
        assert!(!f.at_ground());
        run(&mut f, &snowman[1..]);
        assert!(f.at_ground());
    }

    #[test]
    fn erase_requests_repaint() {
        let mut f = Filter::new(10);
        assert!(run(&mut f, b"\x1b[2J").1.repaint);
        assert!(run(&mut f, b"\x1b[J").1.repaint);
        assert!(run(&mut f, b"\x1b[0J").1.repaint);
        assert!(!run(&mut f, b"\x1b[1J").1.repaint);
        assert!(!run(&mut f, b"\x1b[2K").1.repaint);
    }

    #[test]
    fn alt_screen_tracking() {
        let mut f = Filter::new(10);
        let (_, ev) = run(&mut f, b"\x1b[?1049h");
        assert!(ev.repaint && !ev.fixup);
        assert!(f.alt_screen());
        let (_, ev) = run(&mut f, b"\x1b[?1049l");
        assert!(ev.repaint && ev.fixup);
        assert!(!f.alt_screen());
    }

    #[test]
    fn tracks_autowrap() {
        let mut f = Filter::new(10);
        run(&mut f, b"\x1b[?7l");
        assert!(!f.autowrap());
        run(&mut f, b"\x1b[?25;7h");
        assert!(f.autowrap());
    }

    #[test]
    fn reset_restores_region() {
        let mut f = Filter::new(12);
        let (out, ev) = run(&mut f, b"\x1bc");
        assert_eq!(out, b"\x1bc\x1b[1;12r");
        assert!(ev.repaint);
    }

    #[test]
    fn osc_strings_pass_through() {
        let mut f = Filter::new(10);
        let (out, _) = run(&mut f, b"\x1b]0;title\x07x\x1b]2;t\x1b\\y");
        assert_eq!(out, b"\x1b]0;title\x07x\x1b]2;t\x1b\\y");
        assert!(f.at_ground());
    }

    #[test]
    fn parses_cursor_report() {
        let mut buf = b"ab\x1b[12;34Rcd".to_vec();
        assert_eq!(take_cursor_report(&mut buf), Some((12, 34)));
        assert_eq!(buf, b"abcd");
        let mut buf = b"\x1b[A".to_vec();
        assert_eq!(take_cursor_report(&mut buf), None);
        assert_eq!(buf, b"\x1b[A");
    }
}
