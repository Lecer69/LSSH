use crate::config::Config;
use crate::stats::{Stats, human_rate, human_uptime};

pub type Rgb = (u8, u8, u8);

/// Partial block glyphs used to smooth the metre fill (btop-like).
const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// A run of text that may be coloured.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    text: String,
    color: Option<Rgb>,
}

impl Piece {
    fn plain(text: impl Into<String>) -> Self {
        Piece {
            text: text.into(),
            color: None,
        }
    }

    fn colored(text: impl Into<String>, color: Rgb) -> Self {
        Piece {
            text: text.into(),
            color: Some(color),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BarStyle {
    Smooth,
    Block,
    Line,
}

impl BarStyle {
    fn parse(name: &str) -> BarStyle {
        match name.trim().to_ascii_lowercase().as_str() {
            "block" => BarStyle::Block,
            "line" => BarStyle::Line,
            _ => BarStyle::Smooth,
        }
    }
}

/// Colour palette for the stats bar.
#[derive(Debug, Clone)]
pub struct Theme {
    pub label: Rgb,
    pub name: Rgb,
    pub cpu: Rgb,
    pub mem: Rgb,
    pub disk: Rgb,
    pub net_down: Rgb,
    pub net_up: Rgb,
    pub warn: Rgb,
    pub crit: Rgb,
    pub frame: Rgb,
    /// When false, all ANSI colour is omitted.
    pub color: bool,
}

impl Theme {
    pub fn from_name(name: &str) -> Theme {
        match name.trim().to_ascii_lowercase().as_str() {
            "mono" => Theme {
                color: false,
                ..Theme::btop()
            },
            "neon" => Theme {
                label: (99, 110, 140),
                name: (255, 255, 255),
                cpu: (0, 255, 179),
                mem: (255, 0, 200),
                disk: (0, 200, 255),
                net_down: (0, 255, 179),
                net_up: (255, 220, 0),
                warn: (255, 200, 0),
                crit: (255, 60, 90),
                frame: (60, 70, 100),
                color: true,
            },
            _ => Theme::btop(),
        }
    }

    /// A btop-flavoured palette.
    pub fn btop() -> Theme {
        Theme {
            label: (108, 122, 150),
            name: (192, 202, 245),
            cpu: (94, 235, 196),
            mem: (187, 154, 247),
            disk: (122, 162, 247),
            net_down: (94, 235, 196),
            net_up: (224, 175, 104),
            warn: (224, 175, 104),
            crit: (247, 118, 142),
            frame: (86, 95, 137),
            color: true,
        }
    }

    fn usage(&self, pct: f64, base: Rgb) -> Rgb {
        if !self.color {
            return base;
        }
        if pct >= 85.0 {
            self.crit
        } else if pct >= 65.0 {
            self.warn
        } else {
            base
        }
    }
}

fn fg(color: Rgb) -> String {
    format!("\x1b[38;2;{};{};{}m", color.0, color.1, color.2)
}

const RESET: &str = "\x1b[0m";

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let blend = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    (blend(a.0, b.0), blend(a.1, b.1), blend(a.2, b.2))
}

fn lighten(color: Rgb, t: f64) -> Rgb {
    mix(color, (255, 255, 255), t)
}

/// Gradient across a filled metre: darker on the left, brighter on the right.
fn grad(base: Rgb, index: usize, count: usize) -> Rgb {
    if count <= 1 {
        return base;
    }
    let t = index as f64 / (count - 1) as f64;
    mix(base, lighten(base, 0.5), t)
}

fn render(pieces: &[Piece], color: bool) -> String {
    let mut out = String::new();
    for piece in pieces {
        match piece.color {
            Some(c) if color => {
                out.push_str(&fg(c));
                out.push_str(&piece.text);
                out.push_str(RESET);
            }
            _ => out.push_str(&piece.text),
        }
    }
    out
}

fn width_of(pieces: &[Piece]) -> usize {
    pieces.iter().map(|p| p.text.chars().count()).sum()
}

/// Truncate a piece list to `width` display columns.
fn fit(pieces: &[Piece], width: usize) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut remaining = width;
    for piece in pieces {
        if remaining == 0 {
            break;
        }
        let len = piece.text.chars().count();
        if len <= remaining {
            out.push(piece.clone());
            remaining -= len;
        } else {
            out.push(Piece {
                text: piece.text.chars().take(remaining).collect(),
                color: piece.color,
            });
            remaining = 0;
        }
    }
    out
}

fn concat_groups(groups: Vec<Vec<Piece>>, sep: &str, theme: &Theme) -> Vec<Piece> {
    let mut out = Vec::new();
    for group in groups {
        if group.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Piece::colored(sep, theme.frame));
        }
        out.extend(group);
    }
    out
}

/// Build the filled/empty cells of one metre.
fn meter_cells(pct: f64, width: usize, color: Rgb, style: BarStyle, theme: &Theme) -> Vec<Piece> {
    if width == 0 {
        return Vec::new();
    }
    let mut cells = Vec::new();
    match style {
        BarStyle::Smooth => {
            let eighths = ((pct / 100.0) * width as f64 * 8.0).round() as usize;
            let full = (eighths / 8).min(width);
            let rem = eighths % 8;
            let partial = full < width && rem > 0;
            let filled = full + usize::from(partial);
            for i in 0..full {
                cells.push(Piece::colored("█", grad(color, i, filled)));
            }
            if partial {
                cells.push(Piece::colored(PARTIAL[rem], grad(color, full, filled)));
            }
            let empty = width - filled;
            if empty > 0 {
                cells.push(Piece::colored("░".repeat(empty), theme.frame));
            }
        }
        BarStyle::Block => {
            let filled = (((pct / 100.0) * width as f64).round() as usize).min(width);
            for i in 0..filled {
                cells.push(Piece::colored("█", grad(color, i, filled)));
            }
            if width - filled > 0 {
                cells.push(Piece::colored("░".repeat(width - filled), theme.frame));
            }
        }
        BarStyle::Line => {
            let filled = (((pct / 100.0) * width as f64).round() as usize).min(width);
            for i in 0..filled {
                cells.push(Piece::colored("━", grad(color, i, filled)));
            }
            if width - filled > 0 {
                cells.push(Piece::colored("─".repeat(width - filled), theme.frame));
            }
        }
    }
    cells
}

fn empty_track(width: usize, style: BarStyle) -> String {
    match style {
        BarStyle::Line => "─".repeat(width),
        _ => "░".repeat(width),
    }
}

fn percent_meter(
    label: &str,
    value: Option<f64>,
    width: usize,
    base: Rgb,
    style: BarStyle,
    theme: &Theme,
) -> Vec<Piece> {
    match value {
        Some(pct) => {
            let pct = pct.clamp(0.0, 100.0);
            let color = theme.usage(pct, base);
            let mut pieces = vec![Piece::colored(format!("{label} "), theme.label)];
            pieces.extend(meter_cells(pct, width, color, style, theme));
            pieces.push(Piece::plain(format!(" {pct:>3.0}%")));
            pieces
        }
        None => vec![
            Piece::colored(format!("{label} "), theme.label),
            Piece::colored(empty_track(width, style), theme.frame),
            Piece::plain(" n/a"),
        ],
    }
}

fn net_pieces(stats: Option<&Stats>, theme: &Theme) -> Vec<Piece> {
    let rx = stats
        .and_then(|s| s.rx_rate)
        .map(human_rate)
        .unwrap_or_else(|| "--".to_string());
    let tx = stats
        .and_then(|s| s.tx_rate)
        .map(human_rate)
        .unwrap_or_else(|| "--".to_string());
    vec![
        Piece::colored("NET ", theme.label),
        Piece::colored("↓", theme.net_down),
        Piece::plain(" "),
        Piece::colored(rx, theme.net_down),
        Piece::colored("  ↑", theme.net_up),
        Piece::plain(" "),
        Piece::colored(tx, theme.net_up),
    ]
}

fn identity_pieces(
    name: &str,
    user: &str,
    host: &str,
    stats: Option<&Stats>,
    cfg: &Config,
    theme: &Theme,
) -> Vec<Piece> {
    let mut pieces = Vec::new();
    if cfg.show_name {
        pieces.push(Piece::colored(format!(" {name} "), theme.name));
    }
    pieces.push(Piece::colored(format!("{user}@{host}"), theme.label));
    if cfg.show_os
        && let Some(os) = stats.and_then(|s| s.os.clone())
    {
        pieces.push(Piece::plain("  "));
        pieces.push(Piece::colored(os, theme.label));
    }
    pieces
}

fn meter_pieces(
    stats: Option<&Stats>,
    cfg: &Config,
    theme: &Theme,
    terminal_width: usize,
) -> Vec<Piece> {
    let mut bar_count = 0;
    if cfg.show_cpu {
        bar_count += 1;
    }
    if cfg.show_memory {
        bar_count += 1;
    }
    if cfg.show_disk {
        bar_count += 1;
    }

    let net = if cfg.show_network {
        net_pieces(stats, theme)
    } else {
        Vec::new()
    };
    let net_width = width_of(&net);

    // Fixed text per bar: "CPU " (4) + " 100%" (5) = 9 columns. Two spaces
    // between groups. Give every bar an equal share of what is left, so the
    // metres stretch with the terminal just like btop.
    let group_count = bar_count + usize::from(cfg.show_network);
    let fixed = 9 * bar_count + 2 * group_count.saturating_sub(1) + net_width;
    let bar_width = terminal_width
        .saturating_sub(fixed)
        .checked_div(bar_count)
        .map(|share| share.clamp(4, 60))
        .unwrap_or(0);

    let style = BarStyle::parse(&cfg.bar_style);
    let mut groups: Vec<Vec<Piece>> = Vec::new();
    if cfg.show_cpu {
        groups.push(percent_meter(
            "CPU",
            stats.and_then(|s| s.cpu),
            bar_width,
            theme.cpu,
            style,
            theme,
        ));
    }
    if cfg.show_memory {
        let pct = stats.and_then(|s| match (s.mem_used, s.mem_total) {
            (Some(used), Some(total)) if total > 0 => Some(used as f64 / total as f64 * 100.0),
            _ => None,
        });
        groups.push(percent_meter(
            "MEM", pct, bar_width, theme.mem, style, theme,
        ));
    }
    if cfg.show_disk {
        let pct = stats.and_then(|s| match (s.disk_used, s.disk_total) {
            (Some(used), Some(total)) if total > 0 => Some(used as f64 / total as f64 * 100.0),
            _ => None,
        });
        groups.push(percent_meter(
            "DSK", pct, bar_width, theme.disk, style, theme,
        ));
    }
    if cfg.show_network {
        groups.push(net);
    }
    concat_groups(groups, "  ", theme)
}

fn meta_pieces(stats: Option<&Stats>, cfg: &Config, theme: &Theme) -> Vec<Piece> {
    let mut items: Vec<String> = Vec::new();
    if cfg.show_uptime
        && let Some(up) = stats.and_then(|s| s.uptime)
    {
        items.push(format!("up {}", human_uptime(up)));
    }
    if cfg.show_load
        && let Some((a, b, c)) = stats.and_then(|s| s.load)
    {
        items.push(format!("load {a:.2} {b:.2} {c:.2}"));
    }
    if let Some(cores) = stats.and_then(|s| s.cores) {
        items.push(format!("{cores} cores"));
    }

    let mut pieces = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            pieces.push(Piece::colored("  ·  ", theme.frame));
        }
        pieces.push(Piece::colored(item, theme.label));
    }
    pieces
}

fn frame_line(left: &str, right: &str, label: &str, width: usize, theme: &Theme) -> Vec<Piece> {
    let inner = width.saturating_sub(2);
    let content = if label.is_empty() {
        Vec::new()
    } else {
        vec![Piece::colored(format!("─ {label} "), theme.name)]
    };
    let used = width_of(&content);
    let fill = inner.saturating_sub(used);

    let mut pieces = vec![Piece::colored(left.to_string(), theme.frame)];
    pieces.extend(content);
    pieces.push(Piece::colored("─".repeat(fill), theme.frame));
    pieces.push(Piece::colored(right.to_string(), theme.frame));
    pieces
}

/// Render the bottom stats bar.
///
/// Returns exactly `height` lines (each `<= width` display columns), already
/// containing ANSI colour. `height == 0` returns an empty vector.
#[allow(clippy::too_many_arguments)]
pub fn render_bar(
    stats: Option<&Stats>,
    name: &str,
    user: &str,
    host: &str,
    width: usize,
    height: usize,
    cfg: &Config,
    theme: &Theme,
) -> Vec<String> {
    if height == 0 || width == 0 {
        return Vec::new();
    }

    let identity = identity_pieces(name, user, host, stats, cfg, theme);
    let meters = meter_pieces(stats, cfg, theme, width);
    let meta = meta_pieces(stats, cfg, theme);

    let lines: Vec<Vec<Piece>> = match height {
        1 => vec![concat_groups(vec![identity, meters, meta], "  ·  ", theme)],
        2 => vec![concat_groups(vec![identity, meta], "  ·  ", theme), meters],
        3 => vec![
            frame_line("╭", "╮", name, width, theme),
            meters,
            frame_line("╰", "╯", &meta_plain(&meta), width, theme),
        ],
        _ => vec![
            frame_line("╭", "╮", name, width, theme),
            identity,
            meters,
            frame_line("╰", "╯", &meta_plain(&meta), width, theme),
        ],
    };

    let mut rendered: Vec<String> = lines
        .iter()
        .map(|line| render(&fit(line, width), theme.color))
        .collect();

    while rendered.len() < height {
        rendered.push(String::new());
    }
    rendered
}

fn meta_plain(pieces: &[Piece]) -> String {
    pieces.iter().map(|p| p.text.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn sample_stats() -> Stats {
        Stats {
            cpu: Some(42.0),
            mem_used: Some(6_000_000_000),
            mem_total: Some(16_000_000_000),
            disk_used: Some(40_000_000_000),
            disk_total: Some(100_000_000_000),
            rx_rate: Some(1_500_000.0),
            tx_rate: Some(300_000.0),
            load: Some((0.5, 0.6, 0.7)),
            uptime: Some(123_456),
            os: Some("Linux 6.1 x86_64".into()),
            cores: Some(8),
        }
    }

    fn meter_char_count(s: &str) -> usize {
        strip_ansi(s)
            .chars()
            .filter(|c| "█░▏▎▍▌▋▊▉━─".contains(*c))
            .count()
    }

    #[test]
    fn zero_height_is_empty() {
        let cfg = Config::default();
        let theme = Theme::btop();
        assert!(render_bar(None, "web", "root", "1.2.3.4", 80, 0, &cfg, &theme).is_empty());
    }

    #[test]
    fn produces_exact_height() {
        let cfg = Config::default();
        let theme = Theme::btop();
        let stats = sample_stats();
        for h in 1..=6usize {
            let lines = render_bar(Some(&stats), "web", "root", "1.2.3.4", 100, h, &cfg, &theme);
            assert_eq!(lines.len(), h, "height {h}");
        }
    }

    #[test]
    fn lines_never_exceed_width() {
        let theme = Theme::btop();
        let stats = sample_stats();
        for style in ["smooth", "block", "line"] {
            let cfg = Config {
                bar_style: style.into(),
                ..Config::default()
            };
            for width in [1usize, 5, 20, 40, 80, 120] {
                let lines = render_bar(
                    Some(&stats),
                    "web",
                    "root",
                    "1.2.3.4",
                    width,
                    4,
                    &cfg,
                    &theme,
                );
                for line in lines {
                    let visible = strip_ansi(&line).chars().count();
                    assert!(visible <= width, "{style} width {width}, line '{line}'");
                }
            }
        }
    }

    #[test]
    fn meters_stretch_with_width() {
        let cfg = Config::default();
        let theme = Theme::from_name("mono");
        let stats = sample_stats();
        let narrow = render_bar(Some(&stats), "web", "root", "1.2.3.4", 60, 2, &cfg, &theme);
        let wide = render_bar(Some(&stats), "web", "root", "1.2.3.4", 200, 2, &cfg, &theme);
        let narrow_meters = meter_char_count(&narrow[1]);
        let wide_meters = meter_char_count(&wide[1]);
        assert!(
            wide_meters > narrow_meters,
            "wide={wide_meters} narrow={narrow_meters}"
        );
    }

    #[test]
    fn height_two_shows_uptime() {
        let cfg = Config::default();
        let theme = Theme::from_name("mono");
        let stats = sample_stats();
        let lines = render_bar(Some(&stats), "web", "root", "1.2.3.4", 120, 2, &cfg, &theme);
        let first = strip_ansi(&lines[0]);
        assert!(first.contains("up 1d 10h"), "got: {first}");
    }

    #[test]
    fn line_style_uses_line_glyphs() {
        let cfg = Config {
            bar_style: "line".into(),
            ..Config::default()
        };
        let theme = Theme::from_name("mono");
        let stats = sample_stats();
        let lines = render_bar(Some(&stats), "web", "root", "1.2.3.4", 80, 2, &cfg, &theme);
        let meters = strip_ansi(&lines[1]);
        assert!(
            meters.contains('━') || meters.contains('─'),
            "got: {meters}"
        );
    }

    #[test]
    fn framed_height_has_borders() {
        let cfg = Config::default();
        let theme = Theme::btop();
        let stats = sample_stats();
        let lines = render_bar(Some(&stats), "web", "root", "1.2.3.4", 80, 4, &cfg, &theme);
        assert!(strip_ansi(&lines[0]).starts_with('╭'));
        assert!(strip_ansi(&lines[3]).starts_with('╰'));
    }

    #[test]
    fn mono_theme_has_no_ansi() {
        let cfg = Config::default();
        let theme = Theme::from_name("mono");
        let stats = sample_stats();
        let lines = render_bar(Some(&stats), "web", "root", "1.2.3.4", 80, 3, &cfg, &theme);
        for line in lines {
            assert!(!line.contains('\x1b'), "mono line had ansi: {line:?}");
        }
    }

    #[test]
    fn missing_stats_do_not_panic() {
        let cfg = Config::default();
        let theme = Theme::btop();
        let lines = render_bar(None, "web", "root", "1.2.3.4", 80, 3, &cfg, &theme);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn disabled_segments_are_omitted() {
        let cfg = Config {
            show_cpu: false,
            show_memory: false,
            show_disk: false,
            show_network: false,
            ..Config::default()
        };
        let theme = Theme::from_name("mono");
        let stats = sample_stats();
        let lines = render_bar(Some(&stats), "web", "root", "1.2.3.4", 80, 2, &cfg, &theme);
        let all = lines.join("");
        assert!(!all.contains("CPU"));
        assert!(!all.contains("MEM"));
        assert!(!all.contains("DSK"));
        assert!(!all.contains("NET"));
    }
}
