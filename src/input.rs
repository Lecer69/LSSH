//! Decoding keyboard and mouse input from the local terminal.
//!
//! While the file panel is open, every byte from the terminal goes through
//! [`Parser`], which splits it into keys, mouse events and pastes. Each
//! token keeps its raw bytes so input meant for the remote shell can be
//! forwarded unchanged.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Key {
    pub fn new(code: KeyCode) -> Key {
        Key {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    fn ctrl(c: char) -> Key {
        Key {
            ctrl: true,
            ..Key::new(KeyCode::Char(c))
        }
    }

    /// Apply an xterm modifier parameter (1 + shift/alt/ctrl bits).
    fn with_modifiers(mut self, param: u16) -> Key {
        let bits = param.saturating_sub(1);
        self.shift |= bits & 1 != 0;
        self.alt |= bits & 2 != 0;
        self.ctrl |= bits & 4 != 0;
        self
    }

    /// Whether this is Ctrl+`c` (and nothing else).
    pub fn is_ctrl(&self, c: char) -> bool {
        self.ctrl && !self.alt && self.code == KeyCode::Char(c)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Release,
    Drag,
    Move,
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mouse {
    pub kind: MouseKind,
    pub button: Button,
    /// 0-based screen position.
    pub col: u16,
    pub row: u16,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mouse {
    /// The same event at another position.
    pub fn at(mut self, col: u16, row: u16) -> Mouse {
        self.col = col;
        self.row = row;
        self
    }

    /// xterm button code (without the motion/wheel offsets' encoding).
    fn code(&self, release_as_3: bool) -> u16 {
        let mut code = match (self.kind, self.button) {
            (MouseKind::ScrollUp, _) => 64,
            (MouseKind::ScrollDown, _) => 65,
            (MouseKind::Release, _) if release_as_3 => 3,
            (MouseKind::Move, _) | (_, Button::None) => 3,
            (_, Button::Left) => 0,
            (_, Button::Middle) => 1,
            (_, Button::Right) => 2,
        };
        if matches!(self.kind, MouseKind::Drag | MouseKind::Move) {
            code += 32;
        }
        if self.shift {
            code += 4;
        }
        if self.alt {
            code += 8;
        }
        if self.ctrl {
            code += 16;
        }
        code
    }

    /// Encode for an application, in the reporting mode and encoding it
    /// asked for; `None` when that mode does not report this event.
    pub fn encode(
        &self,
        mode: vt100::MouseProtocolMode,
        encoding: vt100::MouseProtocolEncoding,
    ) -> Option<Vec<u8>> {
        use vt100::MouseProtocolMode as M;
        let wanted = match mode {
            M::None => false,
            M::Press => matches!(self.kind, MouseKind::Press),
            M::PressRelease => !matches!(self.kind, MouseKind::Drag | MouseKind::Move),
            M::ButtonMotion => self.kind != MouseKind::Move,
            M::AnyMotion => true,
        };
        if !wanted {
            return None;
        }
        let (x, y) = (u32::from(self.col) + 1, u32::from(self.row) + 1);
        match encoding {
            vt100::MouseProtocolEncoding::Sgr => {
                let end = if self.kind == MouseKind::Release {
                    'm'
                } else {
                    'M'
                };
                Some(format!("\x1b[<{};{x};{y}{end}", self.code(false)).into_bytes())
            }
            vt100::MouseProtocolEncoding::Utf8 => {
                let mut out = b"\x1b[M".to_vec();
                for value in [u32::from(self.code(true)) + 32, x + 32, y + 32] {
                    let ch = char::from_u32(value.min(2047)).unwrap_or(' ');
                    let mut buf = [0; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                Some(out)
            }
            vt100::MouseProtocolEncoding::Default => {
                let byte = |v: u32| (v + 32).min(255) as u8;
                Some(vec![
                    0x1b,
                    b'[',
                    b'M',
                    byte(u32::from(self.code(true))),
                    byte(x),
                    byte(y),
                ])
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Mouse(Mouse),
    /// Bracketed paste (also what terminals send for dropped files).
    Paste(String),
    /// Anything else: focus reports, unknown sequences.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub event: Event,
    pub raw: Vec<u8>,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Longest escape sequence we wait for before giving up on it.
const MAX_SEQ: usize = 64;

/// Incremental decoder; sequences may be split across reads.
#[derive(Debug, Default)]
pub struct Parser {
    buf: Vec<u8>,
}

enum Step {
    /// Decoded an event from this many bytes.
    Done(Event, usize),
    /// Need more bytes.
    More,
}

impl Parser {
    pub fn new() -> Parser {
        Parser::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Token> {
        self.buf.extend_from_slice(bytes);
        self.drain(false)
    }

    /// Undecoded bytes, e.g. to hand them on when the panel closes.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }

    /// Called when no more input arrived for a moment: whatever is still
    /// buffered (a lone ESC, a cut-off sequence) is decoded as it is.
    pub fn flush(&mut self) -> Vec<Token> {
        self.drain(true)
    }

    fn drain(&mut self, eof: bool) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut at = 0;
        while at < self.buf.len() {
            match parse(&self.buf[at..], eof) {
                Step::Done(event, used) => {
                    let used = used.max(1).min(self.buf.len() - at);
                    tokens.push(Token {
                        event,
                        raw: self.buf[at..at + used].to_vec(),
                    });
                    at += used;
                }
                Step::More => break,
            }
        }
        self.buf.drain(..at);
        tokens
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse(buf: &[u8], eof: bool) -> Step {
    let b = buf[0];
    match b {
        0x1b => parse_escape(buf, eof),
        b'\r' | b'\n' => Step::Done(Event::Key(Key::new(KeyCode::Enter)), 1),
        b'\t' => Step::Done(Event::Key(Key::new(KeyCode::Tab)), 1),
        0x7f | 0x08 => Step::Done(Event::Key(Key::new(KeyCode::Backspace)), 1),
        0x00 => Step::Done(Event::Key(Key::ctrl(' ')), 1),
        0x01..=0x1a => Step::Done(Event::Key(Key::ctrl((b'a' + b - 1) as char)), 1),
        0x1c..=0x1f => Step::Done(Event::Key(Key::ctrl((b'\\' + b - 0x1c) as char)), 1),
        _ => match decode_char(buf, eof) {
            Some((Some(c), used)) => Step::Done(Event::Key(Key::new(KeyCode::Char(c))), used),
            Some((None, used)) => Step::Done(Event::Other, used),
            None => Step::More,
        },
    }
}

/// Decode one UTF-8 character: `Some((None, n))` for invalid bytes, `None`
/// when it is cut off.
fn decode_char(buf: &[u8], eof: bool) -> Option<(Option<char>, usize)> {
    let len = match buf[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Some((None, 1)),
    };
    if buf.len() < len {
        return if eof { Some((None, buf.len())) } else { None };
    }
    match std::str::from_utf8(&buf[..len]) {
        Ok(s) => Some((s.chars().next(), len)),
        Err(_) => Some((None, 1)),
    }
}

fn parse_escape(buf: &[u8], eof: bool) -> Step {
    let Some(&next) = buf.get(1) else {
        return if eof {
            Step::Done(Event::Key(Key::new(KeyCode::Esc)), 1)
        } else {
            Step::More
        };
    };
    match next {
        b'[' => parse_csi(buf, eof),
        b'O' => match buf.get(2) {
            Some(&c) => {
                let code = match c {
                    b'A' => Some(KeyCode::Up),
                    b'B' => Some(KeyCode::Down),
                    b'C' => Some(KeyCode::Right),
                    b'D' => Some(KeyCode::Left),
                    b'H' => Some(KeyCode::Home),
                    b'F' => Some(KeyCode::End),
                    b'M' => Some(KeyCode::Enter),
                    b'P'..=b'S' => Some(KeyCode::F(c - b'P' + 1)),
                    _ => None,
                };
                Step::Done(code.map_or(Event::Other, |c| Event::Key(Key::new(c))), 3)
            }
            None if eof => Step::Done(
                Event::Key(Key {
                    alt: true,
                    ..Key::new(KeyCode::Char('O'))
                }),
                2,
            ),
            None => Step::More,
        },
        // ESC ESC: the first one is a plain Esc.
        0x1b => Step::Done(Event::Key(Key::new(KeyCode::Esc)), 1),
        _ => match parse(&buf[1..], eof) {
            Step::Done(Event::Key(mut key), used) => {
                key.alt = true;
                Step::Done(Event::Key(key), used + 1)
            }
            Step::Done(_, used) => Step::Done(Event::Other, used + 1),
            Step::More => Step::More,
        },
    }
}

fn parse_csi(buf: &[u8], eof: bool) -> Step {
    if buf.starts_with(PASTE_START) {
        let body = &buf[PASTE_START.len()..];
        return match find(body, PASTE_END) {
            Some(end) => {
                let text = String::from_utf8_lossy(&body[..end]).into_owned();
                Step::Done(
                    Event::Paste(text),
                    PASTE_START.len() + end + PASTE_END.len(),
                )
            }
            None if eof && body.len() > 1 << 20 => Step::Done(
                Event::Paste(String::from_utf8_lossy(body).into_owned()),
                buf.len(),
            ),
            None => Step::More,
        };
    }
    // X10 mouse: ESC [ M b x y.
    if buf.get(2) == Some(&b'M') {
        if buf.len() < 6 {
            return if eof {
                Step::Done(Event::Other, buf.len())
            } else {
                Step::More
            };
        }
        let event = x10_mouse(buf[3], buf[4], buf[5]).map_or(Event::Other, Event::Mouse);
        return Step::Done(event, 6);
    }
    let body = &buf[2..];
    let Some(end) = body.iter().position(|c| (0x40..=0x7e).contains(c)) else {
        if eof || body.len() > MAX_SEQ {
            return Step::Done(Event::Other, buf.len().min(MAX_SEQ + 2));
        }
        return Step::More;
    };
    let fin = body[end];
    let params = &body[..end];
    let used = 2 + end + 1;
    if params.first() == Some(&b'<') {
        let event = sgr_mouse(&params[1..], fin).map_or(Event::Other, Event::Mouse);
        return Step::Done(event, used);
    }
    let text = std::str::from_utf8(params).unwrap_or("");
    let fields: Vec<&str> = text.split(';').collect();
    let num = |i: usize| -> u16 {
        fields
            .get(i)
            .and_then(|f| f.split(':').next())
            .and_then(|f| f.parse().ok())
            .unwrap_or(0)
    };
    let modifiers = num(1);
    let code = match fin {
        b'A' => Some(KeyCode::Up),
        b'B' => Some(KeyCode::Down),
        b'C' => Some(KeyCode::Right),
        b'D' => Some(KeyCode::Left),
        b'H' => Some(KeyCode::Home),
        b'F' => Some(KeyCode::End),
        b'Z' => Some(KeyCode::BackTab),
        b'P'..=b'S' if text.is_empty() || text.starts_with("1;") => {
            Some(KeyCode::F(fin - b'P' + 1))
        }
        b'~' => match num(0) {
            1 | 7 => Some(KeyCode::Home),
            2 => Some(KeyCode::Insert),
            3 => Some(KeyCode::Delete),
            4 | 8 => Some(KeyCode::End),
            5 => Some(KeyCode::PageUp),
            6 => Some(KeyCode::PageDown),
            n @ 11..=15 => Some(KeyCode::F((n - 10) as u8)),
            n @ 17..=21 => Some(KeyCode::F((n - 11) as u8)),
            n @ 23..=24 => Some(KeyCode::F((n - 12) as u8)),
            _ => None,
        },
        b'u' => return Step::Done(kitty_key(&fields), used),
        _ => None,
    };
    let event = match code {
        Some(code) => Event::Key(Key::new(code).with_modifiers(modifiers.max(1))),
        None => Event::Other,
    };
    Step::Done(event, used)
}

/// A key in the kitty keyboard protocol: `CSI code[:alts] ; mods[:event] u`.
fn kitty_key(fields: &[&str]) -> Event {
    let part =
        |i: usize, j: usize| -> Option<u32> { fields.get(i)?.split(':').nth(j)?.parse().ok() };
    let Some(code) = part(0, 0) else {
        return Event::Other;
    };
    // Key release events are of no interest.
    if part(1, 1) == Some(3) {
        return Event::Other;
    }
    let mods = part(1, 0).unwrap_or(1) as u16;
    let key = match code {
        13 => KeyCode::Enter,
        9 => KeyCode::Tab,
        127 | 8 => KeyCode::Backspace,
        27 => KeyCode::Esc,
        c => match char::from_u32(c) {
            // The code is the unshifted key; Shift makes it upper case.
            Some(c) if !c.is_control() && (mods.saturating_sub(1) & 1 != 0) => {
                KeyCode::Char(c.to_uppercase().next().unwrap_or(c))
            }
            Some(c) if !c.is_control() => KeyCode::Char(c),
            _ => return Event::Other,
        },
    };
    Event::Key(Key::new(key).with_modifiers(mods))
}

fn button_event(code: u16, col: u16, row: u16, release: bool) -> Option<Mouse> {
    let shift = code & 4 != 0;
    let alt = code & 8 != 0;
    let ctrl = code & 16 != 0;
    let motion = code & 32 != 0;
    let wheel = code & 64 != 0;
    let low = code & 3;
    let (kind, button) = if wheel {
        match low {
            0 => (MouseKind::ScrollUp, Button::None),
            1 => (MouseKind::ScrollDown, Button::None),
            // Horizontal scrolling is not used.
            _ => return None,
        }
    } else {
        let button = match low {
            0 => Button::Left,
            1 => Button::Middle,
            2 => Button::Right,
            _ => Button::None,
        };
        let kind = if release {
            MouseKind::Release
        } else if motion {
            if button == Button::None {
                MouseKind::Move
            } else {
                MouseKind::Drag
            }
        } else if button == Button::None {
            MouseKind::Release
        } else {
            MouseKind::Press
        };
        (kind, button)
    };
    Some(Mouse {
        kind,
        button,
        col: col.saturating_sub(1),
        row: row.saturating_sub(1),
        shift,
        alt,
        ctrl,
    })
}

fn sgr_mouse(params: &[u8], fin: u8) -> Option<Mouse> {
    if fin != b'M' && fin != b'm' {
        return None;
    }
    let text = std::str::from_utf8(params).ok()?;
    let mut parts = text.split(';').map(|p| p.parse::<u16>().ok());
    let code = parts.next()??;
    let col = parts.next()??;
    let row = parts.next()??;
    button_event(code, col, row, fin == b'm')
}

fn x10_mouse(b: u8, x: u8, y: u8) -> Option<Mouse> {
    let code = u16::from(b.checked_sub(32)?);
    let col = u16::from(x.checked_sub(32)?);
    let row = u16::from(y.checked_sub(32)?);
    button_event(code, col, row, false)
}

/// Watches raw input for the panel hotkey (Ctrl+N) while it is closed, so
/// every other byte can go to the remote untouched.
#[derive(Debug, Default)]
pub struct Hotkey {
    in_paste: bool,
}

impl Hotkey {
    /// Split `bytes` at the first hotkey press: the bytes before it, and
    /// the bytes after it if it was pressed.
    pub fn split(&mut self, bytes: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            if self.in_paste {
                if rest.starts_with(PASTE_END) {
                    self.in_paste = false;
                    i += PASTE_END.len();
                } else {
                    i += 1;
                }
                continue;
            }
            if rest.starts_with(PASTE_START) {
                self.in_paste = true;
                i += PASTE_START.len();
                continue;
            }
            if rest[0] == 0x0e {
                return (bytes[..i].to_vec(), Some(bytes[i + 1..].to_vec()));
            }
            if let Some(len) = kitty_ctrl_n(rest) {
                return (bytes[..i].to_vec(), Some(bytes[i + len..].to_vec()));
            }
            i += 1;
        }
        (bytes.to_vec(), None)
    }
}

/// Ctrl+N in the kitty keyboard protocol (`CSI 110 ; 5 u`, maybe with
/// alternate keys or an event type); returns its length.
fn kitty_ctrl_n(buf: &[u8]) -> Option<usize> {
    let body = buf.strip_prefix(b"\x1b[110")?;
    let end = body.iter().position(|&c| c == b'u')?;
    if end > 24 {
        return None;
    }
    let params = std::str::from_utf8(&body[..end]).ok()?;
    let mut fields = params.split(';');
    // The code field may continue with `:shifted:base` alternates.
    let alternates = fields.next()?;
    if !(alternates.is_empty() || alternates.starts_with(':')) {
        return None;
    }
    let mods = fields.next()?;
    let mut mods = mods.split(':');
    let ctrl = mods.next()? == "5";
    // Ignore repeat (2) and release (3) events.
    let press = mods.next().is_none_or(|e| e == "1");
    (ctrl && press && fields.next().is_none()).then_some(5 + end + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(bytes: &[u8]) -> Vec<Event> {
        let mut p = Parser::new();
        let mut out: Vec<Event> = p.feed(bytes).into_iter().map(|t| t.event).collect();
        out.extend(p.flush().into_iter().map(|t| t.event));
        out
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(Key::new(code))
    }

    #[test]
    fn plain_keys() {
        assert_eq!(
            events(b"a\r\x7f\t\x0e"),
            vec![
                key(KeyCode::Char('a')),
                key(KeyCode::Enter),
                key(KeyCode::Backspace),
                key(KeyCode::Tab),
                Event::Key(Key::ctrl('n')),
            ]
        );
        assert_eq!(events("é".as_bytes()), vec![key(KeyCode::Char('é'))]);
    }

    #[test]
    fn escape_sequences() {
        assert_eq!(
            events(b"\x1b[A\x1bOB\x1b[5~\x1b[3~\x1b[Z"),
            vec![
                key(KeyCode::Up),
                key(KeyCode::Down),
                key(KeyCode::PageUp),
                key(KeyCode::Delete),
                key(KeyCode::BackTab),
            ]
        );
        let ctrl_up = Key {
            ctrl: true,
            ..Key::new(KeyCode::Up)
        };
        assert_eq!(events(b"\x1b[1;5A"), vec![Event::Key(ctrl_up)]);
        let alt_x = Key {
            alt: true,
            ..Key::new(KeyCode::Char('x'))
        };
        assert_eq!(events(b"\x1bx"), vec![Event::Key(alt_x)]);
    }

    #[test]
    fn lone_escape_waits_for_flush() {
        let mut p = Parser::new();
        assert!(p.feed(b"\x1b").is_empty());
        assert_eq!(p.flush()[0].event, key(KeyCode::Esc));
        // Split sequences are joined.
        assert!(p.feed(b"\x1b[").is_empty());
        assert_eq!(p.feed(b"B")[0].event, key(KeyCode::Down));
    }

    #[test]
    fn kitty_keys() {
        assert_eq!(events(b"\x1b[110;5u"), vec![Event::Key(Key::ctrl('n'))]);
        assert_eq!(events(b"\x1b[27u"), vec![key(KeyCode::Esc)]);
        assert_eq!(events(b"\x1b[97;1:3u"), vec![Event::Other]);
        let shift_a = Key {
            shift: true,
            ..Key::new(KeyCode::Char('A'))
        };
        assert_eq!(events(b"\x1b[97;2u"), vec![Event::Key(shift_a)]);
    }

    #[test]
    fn mouse_events() {
        let got = events(b"\x1b[<0;5;3M\x1b[<0;5;3m\x1b[<65;1;1M\x1b[<32;7;3M");
        let Event::Mouse(press) = got[0] else {
            panic!("expected mouse")
        };
        assert_eq!(
            (press.kind, press.button, press.col, press.row),
            (MouseKind::Press, Button::Left, 4, 2)
        );
        let Event::Mouse(release) = got[1] else {
            panic!()
        };
        assert_eq!(release.kind, MouseKind::Release);
        let Event::Mouse(wheel) = got[2] else {
            panic!()
        };
        assert_eq!(wheel.kind, MouseKind::ScrollDown);
        let Event::Mouse(drag) = got[3] else { panic!() };
        assert_eq!((drag.kind, drag.col), (MouseKind::Drag, 6));
    }

    #[test]
    fn mouse_is_reencoded_for_the_remote() {
        use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
        let Event::Mouse(m) = events(b"\x1b[<0;10;4M")[0] else {
            panic!()
        };
        let moved = m.at(2, 3);
        assert_eq!(
            moved.encode(M::PressRelease, E::Sgr).unwrap(),
            b"\x1b[<0;3;4M"
        );
        assert_eq!(
            moved.encode(M::PressRelease, E::Default).unwrap(),
            b"\x1b[M\x20\x23\x24"
        );
        assert_eq!(moved.encode(M::None, E::Sgr), None);
        let Event::Mouse(drag) = events(b"\x1b[<32;10;4M")[0] else {
            panic!()
        };
        assert_eq!(drag.encode(M::PressRelease, E::Sgr), None);
        assert!(drag.encode(M::ButtonMotion, E::Sgr).is_some());
    }

    #[test]
    fn pastes_are_collected() {
        let mut p = Parser::new();
        assert!(p.feed(b"\x1b[200~'/tmp/a b.txt' ").is_empty());
        let tokens = p.feed(b"\x1b[201~x");
        assert_eq!(tokens[0].event, Event::Paste("'/tmp/a b.txt' ".into()));
        assert!(tokens[0].raw.starts_with(PASTE_START) && tokens[0].raw.ends_with(PASTE_END));
        assert_eq!(tokens[1].event, key(KeyCode::Char('x')));
    }

    #[test]
    fn hotkey_splits_input() {
        let mut h = Hotkey::default();
        assert_eq!(h.split(b"ls\r"), (b"ls\r".to_vec(), None));
        assert_eq!(h.split(b"ab\x0ecd"), (b"ab".to_vec(), Some(b"cd".to_vec())));
        assert_eq!(h.split(b"\x1b[110;5u"), (vec![], Some(vec![])));
        assert_eq!(h.split(b"\x1b[110;5:3u").1, None, "release is ignored");
        assert_eq!(h.split(b"\x1b[110;1u").1, None, "plain n");
        // Ctrl+N inside a paste is just pasted.
        assert_eq!(h.split(b"\x1b[200~a\x0eb\x1b[201~").1, None);
        assert_eq!(h.split(b"\x1b[200~a").1, None);
        assert_eq!(h.split(b"\x0e\x1b[201~").1, None);
        assert_eq!(h.split(b"\x0e").1, Some(vec![]));
    }
}
