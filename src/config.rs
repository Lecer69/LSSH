use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::out;
use crate::proxy::Proxy;
use crate::store;
use crate::ui;

/// User-tunable settings loaded from `~/.config/lssh/config.toml`.
///
/// Every field has a default, so a missing or partial file is fine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// How often the remote stats are refreshed, in milliseconds (200-60000).
    pub update_interval_ms: u64,
    /// Number of lines reserved at the bottom for the stats bar (0 disables).
    pub bar_height: u16,
    /// Blank lines between the shell and the stats bar (0-3).
    pub bar_spacing: u16,
    /// Colour theme: "btop", "neon" or "mono".
    pub theme: String,
    /// Meter style: "smooth" (partial blocks), "block" or "line".
    pub bar_style: String,
    /// Show the saved server name in the bar.
    pub show_name: bool,
    /// Show the remote OS in the bar.
    pub show_os: bool,
    /// Show the CPU usage meter.
    pub show_cpu: bool,
    /// Show the memory usage meter.
    pub show_memory: bool,
    /// Show the disk usage meter.
    pub show_disk: bool,
    /// Show the network throughput meter.
    pub show_network: bool,
    /// Show the load average.
    pub show_load: bool,
    /// Show the remote uptime.
    pub show_uptime: bool,
    /// Small history graphs next to the CPU and network meters.
    pub show_sparklines: bool,
    /// Turn the CPU meter red at this percentage (0 never alerts).
    pub alert_cpu: u8,
    /// Turn the memory meter red at this percentage (0 never alerts).
    pub alert_memory: u8,
    /// Turn the disk meter red at this percentage (0 never alerts).
    pub alert_disk: u8,
    /// Record every session as an asciicast file.
    pub record: bool,
    /// Ctrl+N opens the SFTP file panel (false passes Ctrl+N to the remote).
    pub sftp: bool,
    /// Width of the file panel in columns.
    pub sftp_width: u16,
    /// Show hidden files in the panel from the start.
    pub sftp_hidden: bool,
    /// Where the panel downloads to; empty means ~/Downloads.
    pub download_dir: String,
    /// Proxy for commands run with `-x`, e.g. "socks5://127.0.0.1:10808".
    pub proxy: Option<String>,
    /// Per-colour overrides on top of `theme`.
    pub colors: Colors,
}

/// Optional `"#rrggbb"` overrides for the theme colours.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    pub label: Option<String>,
    pub name: Option<String>,
    pub cpu: Option<String>,
    pub mem: Option<String>,
    pub disk: Option<String>,
    pub net_down: Option<String>,
    pub net_up: Option<String>,
    pub warn: Option<String>,
    pub crit: Option<String>,
    pub frame: Option<String>,
}

impl Colors {
    /// Every set colour, with its key.
    pub fn entries(&self) -> [(&'static str, Option<&str>); 10] {
        [
            ("label", self.label.as_deref()),
            ("name", self.name.as_deref()),
            ("cpu", self.cpu.as_deref()),
            ("mem", self.mem.as_deref()),
            ("disk", self.disk.as_deref()),
            ("net_down", self.net_down.as_deref()),
            ("net_up", self.net_up.as_deref()),
            ("warn", self.warn.as_deref()),
            ("crit", self.crit.as_deref()),
            ("frame", self.frame.as_deref()),
        ]
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            update_interval_ms: 1000,
            bar_height: 2,
            bar_spacing: 1,
            theme: "btop".to_string(),
            bar_style: "smooth".to_string(),
            show_name: true,
            show_os: false,
            show_cpu: true,
            show_memory: true,
            show_disk: true,
            show_network: true,
            show_load: true,
            show_uptime: true,
            show_sparklines: true,
            alert_cpu: 85,
            alert_memory: 85,
            alert_disk: 90,
            record: false,
            sftp: true,
            sftp_width: 36,
            sftp_hidden: false,
            download_dir: String::new(),
            proxy: None,
            colors: Colors::default(),
        }
    }
}

impl Config {
    /// Clamp values into a range the renderer can safely handle.
    pub fn normalized(mut self) -> Self {
        self.update_interval_ms = self.update_interval_ms.clamp(200, 60_000);
        self.bar_height = self.bar_height.min(6);
        self.bar_spacing = self.bar_spacing.min(3);
        self.alert_cpu = self.alert_cpu.min(100);
        self.alert_memory = self.alert_memory.min(100);
        self.alert_disk = self.alert_disk.min(100);
        self.sftp_width = self.sftp_width.clamp(20, 200);
        if self.theme.trim().is_empty() {
            self.theme = "btop".to_string();
        }
        if self.proxy.as_deref().is_some_and(|p| p.trim().is_empty()) {
            self.proxy = None;
        }
        self.bar_style = match self.bar_style.trim().to_ascii_lowercase().as_str() {
            "block" => "block".to_string(),
            "line" => "line".to_string(),
            _ => "smooth".to_string(),
        };
        self
    }
}

/// The commented default written to disk on first use.
pub const DEFAULT_TOML: &str = r##"# LSSH configuration
# This file lives in ~/.config/lssh/config.toml.
# Delete it to regenerate the defaults.

# How often remote stats are refreshed, in milliseconds (200-60000).
update_interval_ms = 1000

# Lines reserved at the bottom for the stats bar (0 disables it).
bar_height = 2

# Blank lines between the shell and the stats bar (0-3).
bar_spacing = 1

# Colour theme: "btop", "neon" or "mono".
theme = "btop"

# Meter style: "smooth" (partial blocks, btop-like), "block" or "line".
bar_style = "smooth"

# Which segments appear in the bar.
show_name = true
show_os = false
show_cpu = true
show_memory = true
show_disk = true
show_network = true
show_load = true
show_uptime = true

# Small history graphs next to the CPU and network meters.
show_sparklines = true

# A meter turns red once it reaches this percentage (0 = never).
alert_cpu = 85
alert_memory = 85
alert_disk = 90

# Record every session to ~/.local/share/lssh/recordings as an asciicast
# file (play one back with `asciinema play <file>`). Without this,
# `lssh connect <name> --record` records a single session.
record = false

# Ctrl+N opens an SFTP file panel on the left of the session (see
# `lssh help connect`). Set sftp = false to pass Ctrl+N to the remote
# instead. With bar_height = 0 and sftp = false, sessions are plain ssh.
sftp = true

# Width of the file panel in columns (20-200; it always leaves room for
# the shell). Drag its border or press < and > in it to change it.
sftp_width = 36

# Show hidden (dot) files in the panel from the start (toggle with .).
sftp_hidden = false

# Where the panel downloads to. Empty means ~/Downloads, or the folder
# lssh was started in when there is none.
download_dir = ""

# Proxy for commands run with -x (lssh <name> -x), e.g. a local xray or
# v2ray client: "socks5://host:port" (or just "host:port") or
# "http://host:port". Set it with `lssh proxy set <address>`.
# proxy = "socks5://127.0.0.1:10808"

# Override theme colours with "#rrggbb" values. Keys: label, name, cpu,
# mem, disk, net_down, net_up, warn, crit, frame.
[colors]
# cpu = "#5eebc4"
# crit = "#ff5555"
"##;

/// Path to `config.toml`.
pub fn path() -> Result<PathBuf> {
    Ok(store::config_dir()?.join("config.toml"))
}

/// Read the config, creating the default file if it does not exist yet.
///
/// Never fails: on any error the built-in defaults are returned so a broken
/// config can never prevent an SSH session from starting.
pub fn load() -> Config {
    let path = match path() {
        Ok(path) => path,
        Err(_) => return Config::default().normalized(),
    };

    if !path.exists() {
        if let Err(e) = write_default(&path) {
            warn(&format!(
                "could not write default config to {}: {e}",
                path.display()
            ));
        } else {
            out::success(format_args!("created default config at {}", path.display()));
        }
        return Config::default().normalized();
    }

    match std::fs::read_to_string(&path) {
        Ok(raw) => match parse(&raw) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn(&format!("{} is invalid: {e:#}", path.display()));
                Config::default().normalized()
            }
        },
        Err(e) => {
            warn(&format!("could not read {}: {e}", path.display()));
            Config::default().normalized()
        }
    }
}

/// Parse and validate config text.
pub fn parse(raw: &str) -> Result<Config> {
    let cfg = toml::from_str::<Config>(raw)
        .context("syntax error")?
        .normalized();
    for (key, value) in cfg.colors.entries() {
        if let Some(value) = value
            && ui::parse_hex(value).is_none()
        {
            bail!("colors.{key} = \"{value}\" is not a \"#rrggbb\" colour");
        }
    }
    if let Some(proxy) = &cfg.proxy
        && Proxy::parse(proxy).is_err()
    {
        bail!("proxy = \"{proxy}\" is not a proxy address like socks5://host:port");
    }
    Ok(cfg)
}

fn warn(msg: &str) {
    out::warn(format_args!("{msg} (using defaults)"));
}

/// Write `DEFAULT_TOML` to `path`, private to the user.
pub fn write_default(path: &Path) -> Result<()> {
    store::write_private(path, DEFAULT_TOML)
}

/// Save `proxy` as the `proxy` setting, or comment it out with `None`,
/// keeping the rest of config.toml as it is. Returns the proxy that was
/// set before.
pub fn set_proxy(proxy: Option<&str>) -> Result<Option<String>> {
    set_proxy_at(&path()?, proxy)
}

fn set_proxy_at(path: &Path, proxy: Option<&str>) -> Result<Option<String>> {
    if !path.exists() {
        write_default(path)?;
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let previous = toml::from_str::<toml::Table>(&raw)
        .ok()
        .and_then(|table| table.get("proxy")?.as_str().map(String::from))
        .filter(|proxy| !proxy.trim().is_empty());

    let updated = with_proxy(&raw, proxy);
    if let Err(e) = parse(&updated) {
        bail!(
            "{} is invalid: {e:#}\nfix it with: lssh config edit",
            path.display()
        );
    }
    if updated != raw {
        store::write_private(path, &updated)?;
    }
    Ok(previous)
}

/// Introduces a `proxy` line added to a config that had none.
const PROXY_COMMENT: &str = "# Proxy for commands run with -x; see: lssh help proxy";

/// Config text with its `proxy` line set to `proxy`, or commented out.
/// A commented-out `# proxy = ...` line is reused, so switching the proxy
/// on and off leaves a single line.
fn with_proxy(raw: &str, proxy: Option<&str>) -> String {
    let mut lines: Vec<String> = raw.lines().map(String::from).collect();
    // Top-level keys come before the first [table].
    let top = lines
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    let active = lines[..top].iter().position(|line| sets_proxy(line));
    let commented = lines[..top]
        .iter()
        .position(|line| line.trim_start().strip_prefix('#').is_some_and(sets_proxy));

    match proxy {
        Some(proxy) => {
            let line = format!("proxy = {}", toml::Value::String(proxy.to_string()));
            match active.or(commented) {
                Some(i) => lines[i] = line,
                None if top == lines.len() => {
                    if lines.last().is_some_and(|last| !last.trim().is_empty()) {
                        lines.push(String::new());
                    }
                    lines.extend([PROXY_COMMENT.to_string(), line]);
                }
                None => {
                    // Above the comments that introduce the first table.
                    let mut at = top;
                    while at > 0 && lines[at - 1].trim_start().starts_with('#') {
                        at -= 1;
                    }
                    let mut block = vec![PROXY_COMMENT.to_string(), line, String::new()];
                    if at > 0 && !lines[at - 1].trim().is_empty() {
                        block.insert(0, String::new());
                    }
                    lines.splice(at..at, block);
                }
            }
        }
        None => {
            if let Some(i) = active {
                lines[i] = format!("# {}", lines[i].trim_start());
            }
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Whether a line sets the proxy: `proxy = ...`.
fn sets_proxy(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("proxy")
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert_eq!(cfg.update_interval_ms, 1000);
        assert_eq!(cfg.bar_height, 2);
        assert!(cfg.show_cpu);
        assert!(cfg.show_network);
    }

    #[test]
    fn default_toml_parses_to_defaults() {
        let parsed: Config = toml::from_str(DEFAULT_TOML).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn partial_config_uses_defaults() {
        let parsed: Config = toml::from_str("bar_height = 3\nshow_network = false\n").unwrap();
        assert_eq!(parsed.bar_height, 3);
        assert!(!parsed.show_network);
        assert_eq!(parsed.update_interval_ms, 1000);
        assert!(parsed.show_disk);
    }

    #[test]
    fn colors_are_validated() {
        let cfg = parse("[colors]\ncpu = \"#00ff80\"\n").unwrap();
        assert_eq!(cfg.colors.cpu.as_deref(), Some("#00ff80"));
        assert!(parse("[colors]\ncpu = \"green\"\n").is_err());
        assert!(parse("[colors]\nnope = \"#000000\"\n").is_err());
    }

    #[test]
    fn alerts_are_clamped() {
        let cfg = parse("alert_cpu = 250\n").unwrap();
        assert_eq!(cfg.alert_cpu, 100);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("not_a_key = 1").is_err());
    }

    #[test]
    fn normalization_clamps_values() {
        let cfg = Config {
            update_interval_ms: 5,
            bar_height: 99,
            bar_spacing: 9,
            theme: "  ".into(),
            ..Config::default()
        }
        .normalized();
        assert_eq!(cfg.update_interval_ms, 200);
        assert_eq!(cfg.bar_height, 6);
        assert_eq!(cfg.bar_spacing, 3);
        assert_eq!(cfg.theme, "btop");

        let cfg = Config {
            update_interval_ms: u64::MAX,
            ..Config::default()
        }
        .normalized();
        assert_eq!(cfg.update_interval_ms, 60_000);
    }

    #[test]
    fn bar_style_is_normalized() {
        let cfg = Config {
            bar_style: "  LINE ".into(),
            ..Config::default()
        }
        .normalized();
        assert_eq!(cfg.bar_style, "line");

        let cfg = Config {
            bar_style: "rainbow".into(),
            ..Config::default()
        }
        .normalized();
        assert_eq!(cfg.bar_style, "smooth");
    }

    #[test]
    fn proxy_is_validated() {
        let cfg = parse("proxy = \"127.0.0.1:1080\"\n").unwrap();
        assert_eq!(cfg.proxy.as_deref(), Some("127.0.0.1:1080"));
        assert!(parse("proxy = \"nope\"\n").is_err());
        assert_eq!(parse("proxy = \" \"\n").unwrap().proxy, None);
    }

    #[test]
    fn proxy_line_is_edited_in_place() {
        // The commented example in the default config becomes the setting.
        let set = with_proxy(DEFAULT_TOML, Some("socks5://127.0.0.1:10808"));
        assert_eq!(set.lines().count(), DEFAULT_TOML.lines().count());
        assert!(set.contains("\nproxy = \"socks5://127.0.0.1:10808\"\n"));
        assert_eq!(
            parse(&set).unwrap().proxy.as_deref(),
            Some("socks5://127.0.0.1:10808")
        );

        // Changing it replaces the line, unsetting comments it out, and
        // setting it again reuses that line.
        let changed = with_proxy(&set, Some("http://[::1]:8080"));
        assert_eq!(changed.lines().count(), DEFAULT_TOML.lines().count());
        let unset = with_proxy(&changed, None);
        assert!(unset.contains("\n# proxy = \"http://[::1]:8080\"\n"));
        assert_eq!(parse(&unset).unwrap(), Config::default());
        assert_eq!(with_proxy(&unset, None), unset);
        assert_eq!(with_proxy(&unset, Some("http://[::1]:8080")), changed);
    }

    #[test]
    fn proxy_line_is_added_to_older_configs() {
        let line = "proxy = \"socks5://h:1080\"";
        let old = "bar_height = 3\n\n# Colours.\n[colors]\ncpu = \"#00ff80\"\n";
        let set = with_proxy(old, Some("socks5://h:1080"));
        assert_eq!(
            set,
            format!(
                "bar_height = 3\n\n{PROXY_COMMENT}\n{line}\n\n# Colours.\n[colors]\ncpu = \"#00ff80\"\n"
            )
        );
        assert_eq!(parse(&set).unwrap().colors.cpu.as_deref(), Some("#00ff80"));

        assert_eq!(
            with_proxy("record = true\n[colors]\n", Some("socks5://h:1080")),
            format!("record = true\n\n{PROXY_COMMENT}\n{line}\n\n[colors]\n")
        );
        // Without tables it goes at the end.
        assert_eq!(
            with_proxy("bar_height = 3", Some("socks5://h:1080")),
            format!("bar_height = 3\n\n{PROXY_COMMENT}\n{line}\n")
        );
        assert_eq!(
            with_proxy("", Some("socks5://h:1080")),
            format!("{PROXY_COMMENT}\n{line}\n")
        );
    }

    #[test]
    fn set_proxy_saves_and_reports_the_previous_one() {
        let dir = std::env::temp_dir().join(format!("lssh-proxy-{}", std::process::id()));
        let file = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        // A missing config is created first.
        assert_eq!(set_proxy_at(&file, Some("socks5://h:1080")).unwrap(), None);
        let saved = parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved.proxy.as_deref(), Some("socks5://h:1080"));
        assert_eq!(
            set_proxy_at(&file, None).unwrap().as_deref(),
            Some("socks5://h:1080")
        );
        assert_eq!(set_proxy_at(&file, None).unwrap(), None);

        // A config broken elsewhere is left alone.
        std::fs::write(&file, "theme = \n").unwrap();
        let error = set_proxy_at(&file, Some("socks5://h:1080")).unwrap_err();
        assert!(error.to_string().contains("lssh config edit"), "{error}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theme = \n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_default_roundtrips() {
        let dir = std::env::temp_dir().join(format!("lssh-cfg-{}", std::process::id()));
        let file = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        write_default(&file).unwrap();
        let raw = std::fs::read_to_string(&file).unwrap();
        let parsed: Config = toml::from_str(&raw).unwrap();
        assert_eq!(parsed, Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
