//! Handlers for the commands that manage the server list, connect, and
//! configure lssh. Remote commands live in `remote.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::cli;
use crate::completions;
use crate::config;
use crate::import;
use crate::out;
use crate::password;
use crate::picker;
use crate::session::{self, Session};
use crate::store::{self, Server, Store};
use crate::top;

/// Plain `lssh`: choose a server from a list, or show help when there is
/// nothing to choose from (or no terminal to choose on).
pub fn pick() -> Result<()> {
    let store = Store::open()?;
    if store.servers().is_empty() || !out::interactive() {
        return cli::help(None);
    }
    match picker::pick(&store.by_recent())? {
        Some(name) => connect(&name, false),
        None => Ok(()),
    }
}

fn check_new_name(name: &str) -> Result<()> {
    if cli::is_reserved(name) {
        bail!("'{name}' means something else to lssh, so it can't be a server name");
    }
    store::validate_name(name)
}

/// Resolve a private key path and check that it exists.
fn key_path(key: &str) -> Result<PathBuf> {
    let path = store::expand_path(key)?;
    if !path.is_file() {
        bail!("no key file at {}", path.display());
    }
    Ok(path)
}

pub fn add(name: &str, target: &str, key: Option<&str>) -> Result<()> {
    check_new_name(name)?;
    let mut server = Server::new(name, target)?;
    if let Some(key) = key {
        server.identity = Some(key_path(key)?);
    }

    let mut store = Store::open()?;
    store.add(server.clone())?;
    store.save()?;

    out::success(format_args!(
        "saved {} {}",
        out::name(&server),
        out::target(&server)
    ));
    out::hint(format_args!("connect with: lssh {name}"));
    Ok(())
}

pub fn import(file: Option<&str>) -> Result<()> {
    let path = match file {
        Some(file) => store::expand_path(file)?,
        None => store::home_dir()?.join(".ssh").join("config"),
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let hosts = import::parse_ssh_config(&text);
    if hosts.is_empty() {
        out::info(format_args!("no hosts found in {}", path.display()));
        return Ok(());
    }

    let default_user = std::env::var("USER").ok().filter(|u| !u.is_empty());
    let mut store = Store::open()?;
    let mut added = Vec::new();
    let mut skipped = Vec::new();
    for host in hosts {
        let alias = host.alias.clone();
        match imported_server(host, default_user.as_deref(), &store) {
            Ok(server) => {
                store.add(server.clone())?;
                added.push(server);
            }
            Err(reason) => skipped.push((alias, reason)),
        }
    }
    if !added.is_empty() {
        store.save()?;
    }

    for server in &added {
        out::success(format_args!(
            "imported {} {}",
            out::name(server),
            out::target(server)
        ));
    }
    for (alias, reason) in &skipped {
        out::hint(format_args!("skipped {alias}: {reason}"));
    }
    if added.is_empty() {
        out::info("nothing new to import");
    }
    Ok(())
}

fn imported_server(
    host: import::HostEntry,
    default_user: Option<&str>,
    store: &Store,
) -> std::result::Result<Server, String> {
    if store.contains(&host.alias) {
        return Err("already saved".into());
    }
    check_new_name(&host.alias).map_err(|e| e.to_string())?;
    let user = host
        .user
        .or_else(|| default_user.map(String::from))
        .ok_or("no User given and $USER is not set")?;
    let port = match host.port.as_deref() {
        None | Some("22") => None,
        Some(port) => Some(
            port.parse::<u16>()
                .ok()
                .filter(|&p| p > 0)
                .ok_or(format!("invalid port '{port}'"))?,
        ),
    };
    Ok(Server {
        host: host.hostname.unwrap_or_else(|| host.alias.clone()),
        name: host.alias,
        user,
        port,
        // ssh expands `~` and `%` tokens in IdentityFile itself.
        identity: host.identity.map(PathBuf::from),
        last_used: None,
    })
}

pub fn list(names_only: bool) -> Result<()> {
    let store = Store::open()?;
    let servers = store.by_recent();
    if names_only {
        for server in servers {
            println!("{}", server.name);
        }
        return Ok(());
    }
    if servers.is_empty() {
        out::info("no servers saved yet");
        out::hint("add one with: lssh add <name> <user@host>");
        out::hint("or import ~/.ssh/config with: lssh import");
        return Ok(());
    }

    let now = store::now();
    let name_width = column_width(servers.iter().map(|s| s.name.chars().count()));
    let address_width = column_width(servers.iter().map(|s| s.address().chars().count()));
    println!("{}", "Servers".bold());
    for server in servers {
        let name_pad = " ".repeat(name_width - server.name.chars().count());
        let address_pad = " ".repeat(address_width - server.address().chars().count());
        let mut line = format!(
            "  {}{name_pad}   {}{address_pad}   {}",
            out::name(server),
            out::target(server),
            out::ago(server.last_used, now).dimmed()
        );
        if let Some(identity) = &server.identity {
            line.push_str(&format!(
                "   {}",
                format!("key {}", identity.display()).dimmed()
            ));
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}

fn column_width(widths: impl Iterator<Item = usize>) -> usize {
    widths.max().unwrap_or(0)
}

pub fn connect(name: &str, record: bool) -> Result<()> {
    let mut store = Store::open()?;
    let server = if name == "-" {
        store.last_used()?
    } else {
        store.get(name)?
    }
    .clone();
    store.touch(&server.name)?;
    store.save()?;

    // Load (and create, on first run) the user config.
    let config = config::load();
    out::step(format_args!(
        "connecting to {} {}",
        out::name(&server),
        out::target(&server)
    ));
    session::run(&Session {
        record: record || config.record,
        server,
        config,
    })
}

pub fn rename(old: &str, new: &str) -> Result<()> {
    check_new_name(new)?;
    let mut store = Store::open()?;
    store.rename(old, new)?;
    store.save()?;

    // Move the saved password along.
    match password::get(old) {
        Ok(None) => {}
        Ok(Some(saved)) => match password::set(new, &saved) {
            Ok(()) => password::delete(old),
            Err(e) => out::warn(format_args!("{e:#} (the saved password was not moved)")),
        },
        Err(e) => out::warn(format_args!("{e:#} (the saved password was not moved)")),
    }
    out::success(format_args!("renamed {old} → {}", new.cyan().bold()));
    Ok(())
}

pub fn remove(name: &str) -> Result<()> {
    let mut store = Store::open()?;
    let server = store.remove(name)?;
    store.save()?;
    password::delete(&server.name);

    out::success(format_args!("removed {}", out::name(&server)));
    Ok(())
}

pub fn passwd(name: &str) -> Result<()> {
    let server = Store::open()?.get(name)?.clone();
    let entered = password::prompt(&format!("new password for {}: ", server.address()))?;
    password::set(&server.name, &entered)?;

    out::success(format_args!(
        "updated the saved password for {}",
        out::name(&server)
    ));
    Ok(())
}

pub fn top(servers: Option<&str>) -> Result<()> {
    let servers = Store::open()?.select(servers.unwrap_or("all"))?;
    top::run(servers, config::load())
}

pub fn completions(shell: &str) -> Result<()> {
    print!("{}", completions::script(shell)?);
    Ok(())
}

pub fn config(action: Option<&str>) -> Result<()> {
    let path = config::path()?;
    match action {
        None => show_config(&path),
        Some("path") => {
            println!("{}", path.display());
            Ok(())
        }
        Some("edit") => edit_config(&path),
        Some(other) => bail!(
            "unknown config action '{other}'\n\
             use: lssh config, lssh config edit or lssh config path"
        ),
    }
}

fn ensure_config(path: &Path) -> Result<()> {
    if !path.exists() {
        config::write_default(path)?;
        out::success("wrote the default config");
    }
    Ok(())
}

/// Print the settings file, creating the default one first if needed.
fn show_config(path: &Path) -> Result<()> {
    ensure_config(path)?;
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    println!(
        "{} {}",
        "Config".bold(),
        path.display().to_string().dimmed()
    );
    println!();
    for line in raw.trim_end().lines() {
        println!("  {}", highlight_toml(line));
    }
    Ok(())
}

/// Open the config in the user's editor, then check what they wrote.
fn edit_config(path: &Path) -> Result<()> {
    ensure_config(path)?;
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|e| !e.trim().is_empty())
        .or_else(|| {
            ["nano", "vim", "vi"]
                .iter()
                .find(|e| in_path(e))
                .map(|e| e.to_string())
        })
        .context("no editor found\nset $EDITOR, e.g. export EDITOR=nano")?;

    // $EDITOR may carry arguments, e.g. "code --wait".
    let mut words = editor.split_whitespace();
    let program = words.next().unwrap_or("vi");
    let status = Command::new(program)
        .args(words)
        .arg(path)
        .status()
        .with_context(|| format!("failed to start '{editor}'"))?;
    if !status.success() {
        bail!("'{editor}' exited with {status}");
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    match config::parse(&raw) {
        Ok(_) => out::success("config saved"),
        Err(e) => {
            out::warn(format_args!("config.toml is invalid: {e:#}"));
            out::hint("lssh uses the defaults until it is fixed: lssh config edit");
        }
    }
    Ok(())
}

fn in_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Light syntax colouring for the config file.
fn highlight_toml(line: &str) -> String {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return line.dimmed().to_string();
    }
    if trimmed.starts_with('[') {
        return line.bold().to_string();
    }
    match line.split_once('=') {
        Some((key, value)) => format!("{}={}", key.cyan(), value.yellow()),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::HostEntry;

    fn host(alias: &str) -> HostEntry {
        HostEntry {
            alias: alias.into(),
            hostname: Some("10.0.0.1".into()),
            user: Some("root".into()),
            ..HostEntry::default()
        }
    }

    #[test]
    fn imports_hosts_with_defaults_and_reasons() {
        let path = std::env::temp_dir().join(format!("lssh-test-imp-{}.json", std::process::id()));
        let mut store = Store::open_at(path).unwrap();
        store
            .add(Server::new("taken", "root@10.0.0.2").unwrap())
            .unwrap();

        let server = imported_server(
            HostEntry {
                port: Some("2222".into()),
                user: None,
                ..host("web")
            },
            Some("me"),
            &store,
        )
        .unwrap();
        assert_eq!(server.address(), "me@10.0.0.1:2222");

        let no_hostname = imported_server(
            HostEntry {
                hostname: None,
                port: Some("22".into()),
                ..host("box.lan")
            },
            None,
            &store,
        )
        .unwrap();
        assert_eq!(no_hostname.address(), "root@box.lan");

        let reason = |entry: HostEntry, user: Option<&str>| {
            imported_server(entry, user, &store).unwrap_err()
        };
        assert_eq!(reason(host("taken"), None), "already saved");
        assert!(reason(host("list"), None).contains("means something else"));
        assert!(
            reason(
                HostEntry {
                    user: None,
                    ..host("db")
                },
                None
            )
            .contains("$USER")
        );
        assert!(
            reason(
                HostEntry {
                    port: Some("x".into()),
                    ..host("db")
                },
                None
            )
            .contains("port")
        );
    }
}
