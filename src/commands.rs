//! One function per CLI command.

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::cli;
use crate::config;
use crate::out;
use crate::password;
use crate::session::{self, Session};
use crate::store::{Server, Store};

pub fn add(name: &str, target: &str) -> Result<()> {
    if cli::find(name).is_some() {
        bail!("'{name}' is an lssh command, so it can't be used as a server name");
    }
    let server = Server::new(name, target)?;

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

pub fn list() -> Result<()> {
    let store = Store::open()?;
    let servers = store.servers();
    if servers.is_empty() {
        out::info("no servers saved yet");
        out::hint("add one with: lssh add <name> <user@host>");
        return Ok(());
    }

    let width = servers
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0);
    println!("{}", "Servers".bold());
    for server in servers {
        let pad = " ".repeat(width - server.name.chars().count());
        println!("  {}{pad}   {}", out::name(server), out::target(server));
    }
    Ok(())
}

pub fn connect(name: &str) -> Result<()> {
    let server = Store::open()?.get(name)?.clone();
    // Load (and create, on first run) the user config.
    let config = config::load();

    out::step(format_args!(
        "connecting to {} {}",
        out::name(&server),
        out::target(&server)
    ));
    let password = session_password(&server)?;

    session::run(&Session {
        name: server.name,
        user: server.user,
        host: server.host,
        password,
        config,
    })
}

/// The password sshpass should replay: the saved one, or a freshly prompted
/// one that is then saved. `None` leaves ssh to ask for it itself.
fn session_password(server: &Server) -> Result<Option<String>> {
    if !session::sshpass_available() {
        out::info(format_args!(
            "{} is not installed, so ssh will ask for the password (it won't be saved)",
            "sshpass".yellow()
        ));
        return Ok(None);
    }

    match password::get(&server.name) {
        Ok(Some(saved)) => return Ok(Some(saved)),
        Ok(None) => {}
        Err(e) => out::warn(format_args!("{e:#}")),
    }

    let entered = password::prompt(&format!("password for {}: ", server.target()))?;
    match password::set(&server.name, &entered) {
        Ok(()) => out::success("password saved to the OS keyring"),
        Err(e) => out::warn(format_args!(
            "{e:#} (the password is used for this session only)"
        )),
    }
    Ok(Some(entered))
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
    if !session::sshpass_available() {
        out::warn(format_args!(
            "{} is not installed, so the saved password won't be used until it is",
            "sshpass".yellow()
        ));
    }

    let entered = password::prompt(&format!("new password for {}: ", server.target()))?;
    password::set(&server.name, &entered)?;

    out::success(format_args!(
        "updated the saved password for {}",
        out::name(&server)
    ));
    Ok(())
}

/// Print the settings file, creating the default one first if needed.
pub fn show_config() -> Result<()> {
    let path = config::path()?;
    if !path.exists() {
        config::write_default(&path)?;
        out::success("wrote the default config");
    }
    let raw = std::fs::read_to_string(&path)
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

/// Light syntax colouring for the config file.
fn highlight_toml(line: &str) -> String {
    if line.trim_start().starts_with('#') {
        return line.dimmed().to_string();
    }
    match line.split_once('=') {
        Some((key, value)) => format!("{}={}", key.cyan(), value.yellow()),
        None => line.to_string(),
    }
}
