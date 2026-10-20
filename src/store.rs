use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// Server metadata. Passwords live in the OS keyring, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    pub user: String,
    pub host: String,
    /// SSH port, when it is not 22.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Private key to log in with, instead of ssh's defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<PathBuf>,
    /// When `lssh` last connected, in seconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<u64>,
}

impl Server {
    /// Build a server from a name and a `user@host[:port]` target.
    pub fn new(name: &str, target: &str) -> Result<Server> {
        validate_name(name)?;
        let (user, host, port) = parse_target(target)?;
        Ok(Server {
            name: name.to_string(),
            user,
            host,
            port,
            identity: None,
            last_used: None,
        })
    }

    /// `user@host`, the destination passed to ssh.
    pub fn target(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// `user@host[:port]`, for display. IPv6 hosts are bracketed when a
    /// port follows.
    pub fn address(&self) -> String {
        match self.port {
            Some(port) => format!("{}@{}:{port}", self.user, bracket_ipv6(&self.host)),
            None => self.target(),
        }
    }

    /// `user@host:path` as understood by scp.
    pub fn scp_path(&self, path: &str) -> String {
        format!("{}@{}:{path}", self.user, bracket_ipv6(&self.host))
    }

    /// `-o` options for the port and key. They work the same for ssh, scp,
    /// sftp and ssh-copy-id, unlike `-p`/`-P`/`-i`.
    pub fn ssh_options(&self) -> Vec<String> {
        let mut options = Vec::new();
        if let Some(port) = self.port {
            options.extend(["-o".to_string(), format!("Port={port}")]);
        }
        if let Some(identity) = &self.identity {
            options.extend([
                "-o".to_string(),
                format!("IdentityFile=\"{}\"", identity.display()),
                "-o".to_string(),
                "IdentitiesOnly=yes".to_string(),
            ]);
        }
        options
    }

    /// Port to connect to.
    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(22)
    }
}

fn bracket_ipv6(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Parse a target into (user, host, port).
///
/// Accepts `user@host`, `user@host:port`, `user@[ipv6]:port` and, when the
/// left side is an IP address, `host@user` (e.g. `11.12.13.14@root`).
pub fn parse_target(target: &str) -> Result<(String, String, Option<u16>)> {
    let invalid = || {
        anyhow!(
            "invalid target '{target}'\n\
             expected user@host or user@host:port (host@user also works for IPs)"
        )
    };
    let (a, b) = target.split_once('@').ok_or_else(invalid)?;
    if a.is_empty() || b.is_empty() || b.contains('@') {
        return Err(invalid());
    }

    let (user, host) = match split_port(a) {
        Ok((host, _)) if host.parse::<IpAddr>().is_ok() => (b, a),
        _ => (a, b),
    };
    let (host, port) = split_port(host)?;
    if host.is_empty() {
        return Err(invalid());
    }
    Ok((user.to_string(), host, port))
}

/// Split `host:port`, `[ipv6]:port` or a bare host / IPv6 address.
fn split_port(host: &str) -> Result<(String, Option<u16>)> {
    let (host, port) = if let Some(rest) = host.strip_prefix('[') {
        let (inside, after) = rest
            .split_once(']')
            .with_context(|| format!("missing ']' in '[{rest}'"))?;
        match after {
            "" => (inside, None),
            _ => (
                inside,
                Some(
                    after
                        .strip_prefix(':')
                        .with_context(|| format!("unexpected '{after}' after ']'"))?,
                ),
            ),
        }
    } else if host.matches(':').count() == 1 {
        let (host, port) = host.split_once(':').unwrap_or((host, ""));
        (host, Some(port))
    } else {
        // No port, or a bare IPv6 address.
        (host, None)
    };

    let port = match port {
        None => None,
        Some(port) => match port.parse::<u16>() {
            Ok(port) if port > 0 => Some(port),
            _ => bail!("invalid port '{port}'"),
        },
    };
    Ok((host.to_string(), port))
}

/// Validate a server name: no whitespace, separators or control chars.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("server name must be 1-64 characters");
    }
    if name
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\' | '@' | ':' | ','))
    {
        bail!("server name must not contain whitespace or any of / \\ @ : ,");
    }
    if name.starts_with('-') {
        bail!("server name must not start with '-'");
    }
    Ok(())
}

/// Directory holding LSSH configuration, honouring `$XDG_CONFIG_HOME`.
pub fn config_dir() -> Result<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// Directory for LSSH data such as recordings, honouring `$XDG_DATA_HOME`.
pub fn data_dir() -> Result<PathBuf> {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    let base = match std::env::var(var) {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home_dir()?.join(fallback),
    };
    Ok(base.join("lssh"))
}

/// Directory for sockets and other short-lived files.
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(std::env::temp_dir)
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("could not determine home directory ($HOME is not set)")
}

/// Expand a leading `~/` and make the path absolute.
pub fn expand_path(path: &str) -> Result<PathBuf> {
    let path = match path.strip_prefix("~/") {
        Some(rest) => home_dir()?.join(rest),
        None => PathBuf::from(path),
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("could not determine the current directory")?
            .join(path))
    }
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The saved servers, backed by `servers.json`.
pub struct Store {
    path: PathBuf,
    servers: Vec<Server>,
}

impl Store {
    /// Load the store from its default location.
    pub fn open() -> Result<Store> {
        Store::open_at(config_dir()?.join("servers.json"))
    }

    /// Load the store from `path`; a missing file is an empty store.
    pub fn open_at(path: PathBuf) -> Result<Store> {
        let servers = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {} (corrupted?)", path.display()))?
        } else {
            Vec::new()
        };
        Ok(Store { path, servers })
    }

    /// Servers in the order they were added.
    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    /// Servers, most recently used first; never-used ones keep their order.
    pub fn by_recent(&self) -> Vec<&Server> {
        let mut servers: Vec<&Server> = self.servers.iter().collect();
        servers.sort_by_key(|s| std::cmp::Reverse(s.last_used));
        servers
    }

    pub fn get(&self, name: &str) -> Result<&Server> {
        self.servers
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| not_found(name))
    }

    fn get_mut(&mut self, name: &str) -> Result<&mut Server> {
        self.servers
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| not_found(name))
    }

    /// The server connected to most recently.
    pub fn last_used(&self) -> Result<&Server> {
        self.servers
            .iter()
            .filter(|s| s.last_used.is_some())
            .max_by_key(|s| s.last_used)
            .context("no server has been connected to yet")
    }

    /// Resolve `all` or a comma-separated list of names.
    pub fn select(&self, names: &str) -> Result<Vec<Server>> {
        if names == "all" {
            if self.servers.is_empty() {
                bail!("no servers saved yet\nadd one with: lssh add <name> <user@host>");
            }
            return Ok(self.servers.clone());
        }
        let mut selected: Vec<Server> = Vec::new();
        for name in names.split(',').filter(|n| !n.is_empty()) {
            let server = self.get(name)?;
            if !selected.contains(server) {
                selected.push(server.clone());
            }
        }
        if selected.is_empty() {
            bail!("no servers given");
        }
        Ok(selected)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.servers.iter().any(|s| s.name == name)
    }

    pub fn add(&mut self, server: Server) -> Result<()> {
        if self.contains(&server.name) {
            bail!(
                "a server named '{name}' already exists\nremove it first with: lssh rm {name}",
                name = server.name
            );
        }
        self.servers.push(server);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<Server> {
        let index = self
            .servers
            .iter()
            .position(|s| s.name == name)
            .ok_or_else(|| not_found(name))?;
        Ok(self.servers.remove(index))
    }

    pub fn rename(&mut self, old: &str, new: &str) -> Result<()> {
        validate_name(new)?;
        if self.contains(new) {
            bail!("a server named '{new}' already exists");
        }
        self.get_mut(old)?.name = new.to_string();
        Ok(())
    }

    /// Record that `name` was just used.
    pub fn touch(&mut self, name: &str) -> Result<()> {
        self.get_mut(name)?.last_used = Some(now());
        Ok(())
    }

    pub fn set_identity(&mut self, name: &str, identity: PathBuf) -> Result<()> {
        self.get_mut(name)?.identity = Some(identity);
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let raw =
            serde_json::to_string_pretty(&self.servers).context("failed to serialize servers")?;
        write_private(&self.path, &raw)
    }
}

fn not_found(name: &str) -> anyhow::Error {
    anyhow!("no server named '{name}'\nsee saved servers with: lssh list")
}

/// Create `dir` (and parents) and make it private to the user (best effort).
pub fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Write `contents` to `path`, readable only by the user: the parent
/// directory is made 0700 (best effort) and the file 0600.
pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
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

    fn target(s: &str) -> (String, String, Option<u16>) {
        parse_target(s).unwrap()
    }

    #[test]
    fn parse_user_at_host() {
        assert_eq!(
            target("root@11.12.13.14"),
            ("root".into(), "11.12.13.14".into(), None)
        );
        assert_eq!(
            target("admin@my.server.example.com"),
            ("admin".into(), "my.server.example.com".into(), None)
        );
    }

    #[test]
    fn parse_host_at_user_for_ips() {
        assert_eq!(
            target("11.12.13.14@root"),
            ("root".into(), "11.12.13.14".into(), None)
        );
        assert_eq!(
            target("11.12.13.14:2222@root"),
            ("root".into(), "11.12.13.14".into(), Some(2222))
        );
    }

    #[test]
    fn parse_ports_and_ipv6() {
        assert_eq!(
            target("root@host:2222"),
            ("root".into(), "host".into(), Some(2222))
        );
        assert_eq!(target("root@::1"), ("root".into(), "::1".into(), None));
        assert_eq!(
            target("root@[2001:db8::1]:22"),
            ("root".into(), "2001:db8::1".into(), Some(22))
        );
        assert_eq!(target("root@[::1]"), ("root".into(), "::1".into(), None));
    }

    #[test]
    fn parse_rejects_bad_targets() {
        for bad in [
            "noatsign",
            "@host",
            "user@",
            "a@b@c",
            "",
            "root@host:",
            "root@host:0",
            "root@host:99999",
            "root@host:ssh",
            "root@[::1",
            "root@[::1]x",
            "root@:22",
        ] {
            assert!(parse_target(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn address_and_scp_path() {
        let mut server = Server::new("web", "root@11.12.13.14").unwrap();
        assert_eq!(server.address(), "root@11.12.13.14");
        server.port = Some(2222);
        assert_eq!(server.address(), "root@11.12.13.14:2222");
        server.host = "2001:db8::1".into();
        assert_eq!(server.address(), "root@[2001:db8::1]:2222");
        assert_eq!(server.scp_path("/tmp/x"), "root@[2001:db8::1]:/tmp/x");
        assert_eq!(server.target(), "root@2001:db8::1");
    }

    #[test]
    fn ssh_options_cover_port_and_key() {
        let mut server = Server::new("web", "root@host:2222").unwrap();
        assert_eq!(server.ssh_options(), ["-o", "Port=2222"]);
        server.identity = Some("/home/me/.ssh/id web".into());
        assert_eq!(
            server.ssh_options(),
            [
                "-o",
                "Port=2222",
                "-o",
                "IdentityFile=\"/home/me/.ssh/id web\"",
                "-o",
                "IdentitiesOnly=yes"
            ]
        );
    }

    #[test]
    fn name_validation() {
        assert!(validate_name("web-server").is_ok());
        assert!(validate_name("srv_1").is_ok());
        assert!(validate_name("1GBServer").is_ok());
        for bad in [
            "",
            "has space",
            "with/slash",
            "with@at",
            "-flag",
            "a:b",
            "a,b",
        ] {
            assert!(validate_name(bad).is_err(), "{bad} should be rejected");
        }
    }

    fn temp_store(tag: &str) -> (PathBuf, Store) {
        let path =
            std::env::temp_dir().join(format!("lssh-test-{tag}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open_at(path.clone()).unwrap();
        (path, store)
    }

    #[test]
    fn save_and_load_roundtrip() {
        let (path, mut store) = temp_store("roundtrip");
        let mut db = Server::new("db", "admin@db.local:2200").unwrap();
        db.identity = Some("/keys/db".into());
        db.last_used = Some(42);
        let servers = vec![Server::new("web", "root@11.12.13.14").unwrap(), db];
        for server in &servers {
            store.add(server.clone()).unwrap();
        }
        store.save().unwrap();
        let loaded = Store::open_at(path.clone()).unwrap();
        assert_eq!(loaded.servers(), servers);

        // Unset optional fields are left out of the file.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.matches("\"port\"").count(), 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "store must be 0600");
        }

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn load_old_formats() {
        let path = std::env::temp_dir().join(format!("lssh-test-old-{}.json", std::process::id()));
        // Old versions had a plaintext password field and no port; ignore it.
        std::fs::write(
            &path,
            r#"[{"name":"web","user":"root","host":"11.12.13.14","password":"secret"}]"#,
        )
        .unwrap();
        let store = Store::open_at(path.clone()).unwrap();
        assert_eq!(
            store.servers(),
            [Server::new("web", "root@11.12.13.14").unwrap()]
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let path = std::env::temp_dir().join("lssh-test-does-not-exist.json");
        let _ = std::fs::remove_file(&path);
        assert!(Store::open_at(path).unwrap().servers().is_empty());
    }

    #[test]
    fn store_add_get_remove_rename() {
        let (_, mut store) = temp_store("ops");
        let web = Server::new("web", "root@11.12.13.14").unwrap();

        store.add(web.clone()).unwrap();
        assert!(
            store.add(web.clone()).is_err(),
            "duplicate names are refused"
        );
        assert_eq!(store.get("web").unwrap(), &web);
        assert!(store.get("db").is_err());

        store.add(Server::new("db", "root@db").unwrap()).unwrap();
        assert!(
            store.rename("web", "db").is_err(),
            "rename onto an existing name"
        );
        assert!(store.rename("web", "bad name").is_err());
        store.rename("web", "www").unwrap();
        assert!(store.get("www").is_ok());

        assert!(store.remove("nope").is_err());
        assert_eq!(store.remove("www").unwrap().host, "11.12.13.14");
        assert_eq!(store.servers().len(), 1);
    }

    #[test]
    fn recency_and_selection() {
        let (_, mut store) = temp_store("recent");
        for name in ["a", "b", "c"] {
            store.add(Server::new(name, "root@host").unwrap()).unwrap();
        }
        assert!(store.last_used().is_err());

        store.get_mut("b").unwrap().last_used = Some(10);
        store.get_mut("c").unwrap().last_used = Some(20);
        assert_eq!(store.last_used().unwrap().name, "c");
        let order: Vec<&str> = store.by_recent().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(order, ["c", "b", "a"]);

        let names = |sel: &str| -> Vec<String> {
            store
                .select(sel)
                .unwrap()
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        assert_eq!(names("all"), ["a", "b", "c"]);
        assert_eq!(names("c,a,c"), ["c", "a"]);
        assert!(store.select("a,zzz").is_err());
        assert!(store.select(",").is_err());
    }
}
