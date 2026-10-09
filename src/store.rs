use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Server metadata. Passwords live in the OS keyring, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    pub user: String,
    pub host: String,
}

/// Parse a target into (user, host).
///
/// Accepts `user@host` and `host@user` (e.g. `11.12.13.14@root`): if the left
/// side is an IP address, it is the host.
pub fn parse_target(target: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = target.split('@').collect();
    if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
        bail!(
            "invalid target '{target}', expected format: user@host (or host@user, e.g. 11.12.13.14@root)"
        );
    }

    let (a, b) = (parts[0], parts[1]);
    if a.parse::<std::net::IpAddr>().is_ok() {
        Ok((b.to_string(), a.to_string()))
    } else {
        Ok((a.to_string(), b.to_string()))
    }
}

/// Validate a server name: no whitespace, separators or control chars.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("server name must be 1-64 characters");
    }
    if name
        .chars()
        .any(|c| c.is_whitespace() || c == '/' || c == '\\' || c == '@' || c.is_control())
    {
        bail!("server name must not contain whitespace, '/', '\\' or '@'");
    }
    Ok(())
}

/// Directory holding LSSH configuration, honouring `$XDG_CONFIG_HOME`.
pub fn config_dir() -> Result<PathBuf> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var("HOME")
                .context("could not determine home directory ($HOME is not set)")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("lssh"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("servers.json"))
}

pub fn load(path: &PathBuf) -> Result<Vec<Server>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let servers = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {} (corrupted?)", path.display()))?;
    Ok(servers)
}

pub fn save(path: &PathBuf, servers: &[Server]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Keep the config private (best effort).
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let raw = serde_json::to_string_pretty(servers).context("failed to serialize servers")?;
    std::fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to restrict permissions on {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_at_host() {
        let (user, host) = parse_target("root@11.12.13.14").unwrap();
        assert_eq!(user, "root");
        assert_eq!(host, "11.12.13.14");
    }

    #[test]
    fn parse_host_at_user_as_in_spec() {
        let (user, host) = parse_target("11.12.13.14@root").unwrap();
        assert_eq!(user, "root");
        assert_eq!(host, "11.12.13.14");
    }

    #[test]
    fn parse_hostname_user_at_host() {
        let (user, host) = parse_target("admin@my.server.example.com").unwrap();
        assert_eq!(user, "admin");
        assert_eq!(host, "my.server.example.com");
    }

    #[test]
    fn parse_rejects_bad_targets() {
        assert!(parse_target("noatsign").is_err());
        assert!(parse_target("@host").is_err());
        assert!(parse_target("user@").is_err());
        assert!(parse_target("a@b@c").is_err());
        assert!(parse_target("").is_err());
    }

    #[test]
    fn name_validation() {
        assert!(validate_name("web-server").is_ok());
        assert!(validate_name("srv_1").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name("with/slash").is_err());
        assert!(validate_name("with@at").is_err());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let path = std::env::temp_dir().join(format!("lssh-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let servers = vec![
            Server {
                name: "web".into(),
                user: "root".into(),
                host: "11.12.13.14".into(),
            },
            Server {
                name: "db".into(),
                user: "admin".into(),
                host: "db.local".into(),
            },
        ];
        save(&path, &servers).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, servers);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "store must be 0600");
        }

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn load_old_format_with_password_field_is_tolerated() {
        let path = std::env::temp_dir().join(format!("lssh-test-old-{}.json", std::process::id()));
        // Old versions had a plaintext password field; ignore it.
        std::fs::write(
            &path,
            r#"[{"name":"web","user":"root","host":"11.12.13.14","password":"secret"}]"#,
        )
        .unwrap();
        let servers = load(&path).unwrap();
        assert_eq!(
            servers,
            vec![Server {
                name: "web".into(),
                user: "root".into(),
                host: "11.12.13.14".into(),
            }]
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let path = std::env::temp_dir().join("lssh-test-does-not-exist.json");
        let _ = std::fs::remove_file(&path);
        assert!(load(&path).unwrap().is_empty());
    }
}
