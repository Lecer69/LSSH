use std::collections::VecDeque;

use crate::config::Config;
use crate::stats::{Stats, human_rate, human_uptime};

pub type Rgb = (u8, u8, u8);

/// Parse a `#rrggbb` colour.
pub fn parse_hex(hex: &str) -> Option<Rgb> {
    let digits = hex.trim().strip_prefix('#')?;
    if digits.len() != 6 || !digits.is_ascii() {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
    Some((channel(0)?, channel(2)?, channel(4)?))
}

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
    /// The configured theme with any `[colors]` overrides applied.
    pub fn from_config(cfg: &Config) -> Theme {
        let mut theme = Theme::from_name(&cfg.theme);
        for (key, value) in cfg.colors.entries() {
            let Some(rgb) = value.and_then(parse_hex) else {
                continue;
            };
            let slot = match key {
                "label" => &mut theme.label,
                "name" => &mut theme.name,
                "cpu" => &mut theme.cpu,
                "mem" => &mut theme.mem,
                "disk" => &mut theme.disk,
                "net_down" => &mut theme.net_down,
                "net_up" => &mut theme.net_up,
                "warn" => &mut theme.warn,
                "crit" => &mut theme.crit,
                _ => &mut theme.frame,
            };
            *slot = rgb;
        }
        theme
    }

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

    /// Meter colour: `crit` from the `alert` percentage, `warn` in the 20
    /// points below it. An alert of 0 never changes the colour.
    fn usage(&self, pct: f64, base: Rgb, alert: u8) -> Rgb {
        if !self.color || alert == 0 {
            return base;
        }
        let alert = f64::from(alert);
        if pct >= alert {
            self.crit
        } else if alert > 20.0 && pct >= alert - 20.0 {
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

/// Recent samples, for the sparklines.
#[derive(Debug, Clone, Default)]
pub struct History {
    cpu: VecDeque<f64>,
    /// Combined receive + transmit rate.
    net: VecDeque<f64>,
}

const HISTORY_LEN: usize = 64;

impl History {
    pub fn push(&mut self, stats: &Stats) {
        fn push_capped(queue: &mut VecDeque<f64>, value: f64) {
            if queue.len() == HISTORY_LEN {
                queue.pop_front();
            }
            queue.push_back(value);
        }
        if let Some(cpu) = stats.cpu {
            push_capped(&mut self.cpu, cpu);
        }
        if let (Some(rx), Some(tx)) = (stats.rx_rate, stats.tx_rate) {
            push_capped(&mut self.net, rx + tx);
        }
    }
}

const SPARK_WIDTH: usize = 8;
const SPARKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// The newest `SPARK_WIDTH` values as a right-aligned sparkline, scaled to
/// `max` (or to the largest value shown when `max` is `None`).
fn sparkline(values: &VecDeque<f64>, max: Option<f64>, color: Rgb) -> Vec<Piece> {
    let shown: Vec<f64> = values
        .iter()
        .rev()
        .take(SPARK_WIDTH)
        .rev()
        .copied()
        .collect();
    let max = max
        .unwrap_or_else(|| shown.iter().copied().fold(0.0, f64::max))
        .max(1.0);
    let graph: String = shown
        .iter()
        .map(|v| SPARKS[((v / max).clamp(0.0, 1.0) * 7.0).round() as usize])
        .collect();
    vec![
        Piece::plain(" ".repeat(SPARK_WIDTH - shown.len())),
        Piece::colored(graph, color),
        Piece::plain(" "),
    ]
}

fn empty_track(width: usize, style: BarStyle) -> String {
    match style {
        BarStyle::Line => "─".repeat(width),
        _ => "░".repeat(width),
    }
}

/// One meter group: label, optional sparkline, cells and percentage. Past
/// the alert threshold the whole group turns red.
#[allow(clippy::too_many_arguments)]
fn percent_meter(
    label: &str,
    value: Option<f64>,
    width: usize,
    base: Rgb,
    alert: u8,
    spark: Vec<Piece>,
    style: BarStyle,
    theme: &Theme,
) -> Vec<Piece> {
    match value {
        Some(pct) => {
            let pct = pct.clamp(0.0, 100.0);
            let color = theme.usage(pct, base, alert);
            let alerting = alert > 0 && pct >= f64::from(alert);
            let text_color = if alerting { theme.crit } else { theme.label };
            let mut pieces = vec![Piece::colored(format!("{label} "), text_color)];
            pieces.extend(spark);
            pieces.extend(meter_cells(pct, width, color, style, theme));
            let pct_text = format!(" {pct:>3.0}%");
            pieces.push(if alerting {
                Piece::colored(pct_text, theme.crit)
            } else {
                Piece::plain(pct_text)
            });
            pieces
        }
        None => {
            let mut pieces = vec![Piece::colored(format!("{label} "), theme.label)];
            pieces.extend(spark);
            pieces.push(Piece::colored(empty_track(width, style), theme.frame));
            pieces.push(Piece::plain(" n/a"));
            pieces
        }
    }
}

fn net_pieces(stats: Option<&Stats>, spark: Vec<Piece>, theme: &Theme) -> Vec<Piece> {
    let rx = stats
        .and_then(|s| s.rx_rate)
        .map(human_rate)
        .unwrap_or_else(|| "--".to_string());
    let tx = stats
        .and_then(|s| s.tx_rate)
        .map(human_rate)
        .unwrap_or_else(|| "--".to_string());
    let mut pieces = vec![Piece::colored("NET ", theme.label)];
    pieces.extend(spark);
    pieces.extend([
        Piece::colored("↓", theme.net_down),
        Piece::plain(" "),
        Piece::colored(rx, theme.net_down),
        Piece::colored("  ↑", theme.net_up),
        Piece::plain(" "),
        Piece::colored(tx, theme.net_up),
    ]);
    pieces
}

fn identity_pieces(
    info: &BarInfo,
    stats: Option<&Stats>,
    cfg: &Config,
    theme: &Theme,
) -> Vec<Piece> {
    let mut pieces = Vec::new();
    if cfg.show_name {
        pieces.push(Piece::colored(format!(" {} ", info.name), theme.name));
    }
    pieces.push(Piece::colored(info.address.to_string(), theme.label));
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
    history: &History,
    cfg: &Config,
    theme: &Theme,
    terminal_width: usize,
) -> Vec<Piece> {
    let bar_count = [cfg.show_cpu, cfg.show_memory, cfg.show_disk]
        .into_iter()
        .filter(|&shown| shown)
        .count();
    let net_width = if cfg.show_network {
        width_of(&net_pieces(stats, Vec::new(), theme))
    } else {
        0
    };

    // Fixed text per bar: "CPU " (4) + " 100%" (5) = 9 columns. Two spaces
    // between groups. Give every bar an equal share of what is left, so the
    // metres stretch with the terminal just like btop.
    let group_count = bar_count + usize::from(cfg.show_network);
    let fixed = 9 * bar_count + 2 * group_count.saturating_sub(1) + net_width;
    let room = |fixed: usize| terminal_width.saturating_sub(fixed);

    // Sparklines only when the metres keep a useful width.
    let spark_count = usize::from(cfg.show_cpu) + usize::from(cfg.show_network);
    let spark_cost = spark_count * (SPARK_WIDTH + 1);
    let sparks = cfg.show_sparklines
        && spark_count > 0
        && match bar_count {
            0 => room(fixed) >= spark_cost,
            n => room(fixed + spark_cost) / n >= 8,
        };
    let fixed = if sparks { fixed + spark_cost } else { fixed };
    let bar_width = room(fixed)
        .checked_div(bar_count)
        .map(|share| share.clamp(4, 60))
        .unwrap_or(0);
    let spark = |values: &VecDeque<f64>, max: Option<f64>, color: Rgb| {
        if sparks {
            sparkline(values, max, color)
        } else {
            Vec::new()
        }
    };

    let style = BarStyle::parse(&cfg.bar_style);
    let mut groups: Vec<Vec<Piece>> = Vec::new();
    if cfg.show_cpu {
        groups.push(percent_meter(
            "CPU",
            stats.and_then(|s| s.cpu),
            bar_width,
            theme.cpu,
            cfg.alert_cpu,
            spark(&history.cpu, Some(100.0), theme.cpu),
            style,
            theme,
        ));
    }
    if cfg.show_memory {
        let pct = stats.and_then(|s| percent(s.mem_used, s.mem_total));
        groups.push(percent_meter(
            "MEM",
            pct,
            bar_width,
            theme.mem,
            cfg.alert_memory,
            Vec::new(),
            style,
            theme,
        ));
    }
    if cfg.show_disk {
        let pct = stats.and_then(|s| percent(s.disk_used, s.disk_total));
        groups.push(percent_meter(
            "DSK",
            pct,
            bar_width,
            theme.disk,
            cfg.alert_disk,
            Vec::new(),
            style,
            theme,
        ));
    }
    if cfg.show_network {
        groups.push(net_pieces(
            stats,
            spark(&history.net, None, theme.net_down),
            theme,
        ));
    }
    concat_groups(groups, "  ", theme)
}

fn percent(used: Option<u64>, total: Option<u64>) -> Option<f64> {
    match (used, total) {
        (Some(used), Some(total)) if total > 0 => Some(used as f64 / total as f64 * 100.0),
        _ => None,
    }
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

/// Who the bar is about.
pub struct BarInfo<'a> {
    pub name: &'a str,
    /// `user@host[:port]`
    pub address: &'a str,
}

/// Render the bottom stats bar.
///
/// Returns exactly `height` lines (each `<= width` display columns), already
/// containing ANSI colour. `height == 0` returns an empty vector.
pub fn render_bar(
    info: &BarInfo,
    stats: Option<&Stats>,
    history: &History,
    width: usize,
    height: usize,
    cfg: &Config,
    theme: &Theme,
) -> Vec<String> {
    if height == 0 || width == 0 {
        return Vec::new();
    }
    let name = info.name;

    let identity = identity_pieces(info, stats, cfg, theme);
    let meters = meter_pieces(stats, history, cfg, theme, width);
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

/// One line naming a server followed by a status message, used by
/// `lssh top` while a server is connecting or after it failed.
pub fn render_status_line(
    info: &BarInfo,
    status: &str,
    error: bool,
    width: usize,
    cfg: &Config,
    theme: &Theme,
) -> String {
    let mut pieces = identity_pieces(info, None, cfg, theme);
    pieces.push(Piece::colored("  ·  ", theme.frame));
    pieces.push(Piece::colored(
        status.to_string(),
        if error { theme.crit } else { theme.label },
    ));
    render(&fit(&pieces, width), theme.color)
}

/// A line of text in the theme's label (or name) colour, cut to `width`.
pub fn render_text(text: &str, emphasis: bool, width: usize, theme: &Theme) -> String {
    let color = if emphasis { theme.name } else { theme.label };
    render(&fit(&[Piece::colored(text, color)], width), theme.color)
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

    fn bar(
        stats: Option<&Stats>,
        width: usize,
        height: usize,
        cfg: &Config,
        theme: &Theme,
    ) -> Vec<String> {
        let info = BarInfo {
            name: "web",
            address: "root@1.2.3.4",
        };
        render_bar(&info, stats, &History::default(), width, height, cfg, theme)
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
        assert!(bar(None, 80, 0, &cfg, &theme).is_empty());
    }

    #[test]
    fn produces_exact_height() {
        let cfg = Config::default();
        let theme = Theme::btop();
        let stats = sample_stats();
        for h in 1..=6usize {
            let lines = bar(Some(&stats), 100, h, &cfg, &theme);
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
                let lines = bar(Some(&stats), width, 4, &cfg, &theme);
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
        let narrow = bar(Some(&stats), 60, 2, &cfg, &theme);
        let wide = bar(Some(&stats), 200, 2, &cfg, &theme);
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
        let lines = bar(Some(&stats), 120, 2, &cfg, &theme);
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
        let lines = bar(Some(&stats), 80, 2, &cfg, &theme);
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
        let lines = bar(Some(&stats), 80, 4, &cfg, &theme);
        assert!(strip_ansi(&lines[0]).starts_with('╭'));
        assert!(strip_ansi(&lines[3]).starts_with('╰'));
    }

    #[test]
    fn mono_theme_has_no_ansi() {
        let cfg = Config::default();
        let theme = Theme::from_name("mono");
        let stats = sample_stats();
        let lines = bar(Some(&stats), 80, 3, &cfg, &theme);
        for line in lines {
            assert!(!line.contains('\x1b'), "mono line had ansi: {line:?}");
        }
    }

    #[test]
    fn missing_stats_do_not_panic() {
        let cfg = Config::default();
        let theme = Theme::btop();
        let lines = bar(None, 80, 3, &cfg, &theme);
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
        let lines = bar(Some(&stats), 80, 2, &cfg, &theme);
        let all = lines.join("");
        assert!(!all.contains("CPU"));
        assert!(!all.contains("MEM"));
        assert!(!all.contains("DSK"));
        assert!(!all.contains("NET"));
    }

    #[test]
    fn parses_hex_colours() {
        assert_eq!(parse_hex("#ff8000"), Some((255, 128, 0)));
        assert_eq!(parse_hex(" #FF8000 "), Some((255, 128, 0)));
        for bad in ["ff8000", "#ff80", "#gg0000", "#ff80000", "#ffé000"] {
            assert_eq!(parse_hex(bad), None, "{bad}");
        }
    }

    #[test]
    fn colour_overrides_apply() {
        let mut cfg = Config::default();
        cfg.colors.cpu = Some("#010203".into());
        let theme = Theme::from_config(&cfg);
        assert_eq!(theme.cpu, (1, 2, 3));
        assert_eq!(theme.mem, Theme::btop().mem);
    }

    #[test]
    fn alert_threshold_turns_the_segment_red() {
        let theme = Theme::btop();
        let red = fg(theme.crit);
        let at = |cpu: f64, alert: u8| {
            let cfg = Config {
                alert_cpu: alert,
                show_memory: false,
                show_disk: false,
                show_network: false,
                ..Config::default()
            };
            let stats = Stats {
                cpu: Some(cpu),
                ..Stats::default()
            };
            bar(Some(&stats), 80, 2, &cfg, &theme)[1].clone()
        };
        assert!(at(50.0, 40).contains(&red));
        assert!(!at(50.0, 90).contains(&red));
        assert!(!at(99.0, 0).contains(&red), "0 disables alerts");
        assert_eq!(theme.usage(55.0, theme.cpu, 70), theme.warn);
    }

    #[test]
    fn sparklines_follow_history() {
        let cfg = Config::default();
        let theme = Theme::from_name("mono");
        let mut history = History::default();
        for cpu in [0.0, 50.0, 100.0] {
            history.push(&Stats {
                cpu: Some(cpu),
                rx_rate: Some(cpu * 10.0),
                tx_rate: Some(0.0),
                ..Stats::default()
            });
        }
        let info = BarInfo {
            name: "web",
            address: "root@1.2.3.4",
        };
        let stats = sample_stats();
        let wide = render_bar(&info, Some(&stats), &history, 200, 2, &cfg, &theme);
        assert!(wide[1].contains("CPU      ▁▅█ "), "got: {}", wide[1]);
        assert!(wide[1].contains("NET      ▁▅█ ↓"), "got: {}", wide[1]);

        // Too narrow: the sparklines make way for the metres.
        let narrow = render_bar(&info, Some(&stats), &history, 70, 2, &cfg, &theme);
        assert!(!narrow[1].contains('▅'), "got: {}", narrow[1]);

        let off = Config {
            show_sparklines: false,
            ..Config::default()
        };
        let plain = render_bar(&info, Some(&stats), &history, 200, 2, &off, &theme);
        assert!(!plain[1].contains('▅'));
    }

    #[test]
    fn history_is_capped() {
        let mut history = History::default();
        for i in 0..200 {
            history.push(&Stats {
                cpu: Some(f64::from(i)),
                ..Stats::default()
            });
        }
        assert_eq!(history.cpu.len(), HISTORY_LEN);
        assert_eq!(history.cpu.back(), Some(&199.0));
    }

    #[test]
    fn status_line_fits() {
        let cfg = Config::default();
        let theme = Theme::from_name("mono");
        let info = BarInfo {
            name: "web",
            address: "root@1.2.3.4",
        };
        let line = render_status_line(&info, "connecting…", false, 30, &cfg, &theme);
        assert!(line.chars().count() <= 30);
        assert!(line.starts_with(" web root@1.2.3.4"));
    }
}
