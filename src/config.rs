use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};

use crate::store;

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
        }
    }
}

impl Config {
    /// Clamp values into a range the renderer can safely handle.
    pub fn normalized(mut self) -> Self {
        self.update_interval_ms = self.update_interval_ms.clamp(200, 60_000);
        self.bar_height = self.bar_height.min(6);
        self.bar_spacing = self.bar_spacing.min(3);
        if self.theme.trim().is_empty() {
            self.theme = "btop".to_string();
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
pub const DEFAULT_TOML: &str = r#"# LSSH configuration
# This file lives in ~/.config/lssh/config.toml.
# Delete it to regenerate the defaults.

# How often remote stats are refreshed, in milliseconds (200-60000).
update_interval_ms = 1000

# Lines reserved at the bottom for the stats bar.
# 0 disables the bar entirely (plain ssh, no stats, no scroll region).
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
"#;

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
            println!(
                "{} created default config at {}",
                "✦".cyan().bold(),
                path.display().to_string().dimmed()
            );
        }
        return Config::default().normalized();
    }

    match std::fs::read_to_string(&path) {
        Ok(raw) => match toml::from_str::<Config>(&raw) {
            Ok(cfg) => cfg.normalized(),
            Err(e) => {
                warn(&format!("{} is invalid: {e}", path.display()));
                Config::default().normalized()
            }
        },
        Err(e) => {
            warn(&format!("could not read {}: {e}", path.display()));
            Config::default().normalized()
        }
    }
}

fn warn(msg: &str) {
    eprintln!("{} {msg} (using defaults)", "⚠".yellow().bold());
}

/// Create the config directory (0700) and write `DEFAULT_TOML` (0600).
pub fn write_default(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    std::fs::write(path, DEFAULT_TOML)
        .with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
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
