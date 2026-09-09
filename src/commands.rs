use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::store::{self, Server};

pub fn add(path: &PathBuf, name: &str, target: &str) -> Result<()> {
    store::validate_name(name)?;
    let (user, host) = store::parse_target(target)?;

    let mut servers = store::load(path)?;
    if servers.iter().any(|s| s.name == name) {
        bail!("server '{name}' already exists (use 'lssh rm {name}' first)");
    }
    servers.push(Server {
        name: name.to_string(),
        user,
        host,
    });
    store::save(path, &servers)?;

    println!("{} added server '{}'", "✔".green().bold(), name.cyan());
    Ok(())
}

pub fn list(path: &PathBuf) -> Result<()> {
    let servers = store::load(path)?;
    if servers.is_empty() {
        println!(
            "{} no servers saved yet, add one with {}",
            "ℹ".cyan().bold(),
            "lssh add <name> <user@host>".yellow()
        );
        return Ok(());
    }

    println!("{}", "saved servers:".bold());
    for s in &servers {
        println!(
            "  {} {} {}@{}",
            "•".cyan(),
            s.name.green().bold(),
            s.user.yellow(),
            s.host.blue()
        );
    }
    Ok(())
}

/// Check that `sshpass` is installed. ssh only reads passwords from the
/// TTY, so sshpass is needed to feed it one.
fn sshpass_available() -> bool {
    std::process::Command::new("sshpass")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

const SSPASS_HINT: &str =
    "sshpass is required for saved passwords, install it (e.g. 'sudo pacman -S sshpass')";

const KEYRING_SERVICE: &str = "lssh";

/// Read the saved password from the OS keyring. Returns `None` if there is
/// no entry or the keyring is unavailable (a warning is printed then).
fn get_saved_password(name: &str) -> Option<String> {
    let entry = match keyring::Entry::new(KEYRING_SERVICE, name) {
        Ok(entry) => entry,
        Err(e) => {
            println!(
                "{} could not access the OS keyring: {e} (password will not be restored)",
                "⚠".yellow().bold()
            );
            return None;
        }
    };
    match entry.get_password() {
        Ok(pw) => Some(pw),
        Err(keyring::Error::NoEntry) => None,
        Err(e) => {
            println!(
                "{} could not read from the OS keyring: {e} (password will not be restored)",
                "⚠".yellow().bold()
            );
            None
        }
    }
}

/// Save the password in the OS keyring. On failure, warn and only use it
/// for this connection.
fn save_password(name: &str, password: &str) {
    let entry = match keyring::Entry::new(KEYRING_SERVICE, name) {
        Ok(entry) => entry,
        Err(e) => {
            println!(
                "{} could not access the OS keyring: {e} (password kept for this session only)",
                "⚠".yellow().bold()
            );
            return;
        }
    };
    match entry.set_password(password) {
        Ok(()) => println!(
            "{} password saved to the OS keyring (won't ask again)",
            "✔".green().bold()
        ),
        Err(e) => println!(
            "{} could not save to the OS keyring: {e} (password kept for this session only)",
            "⚠".yellow().bold()
        ),
    }
}

/// Remove the keyring entry when a server is deleted.
fn delete_saved_password(name: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, name) {
        let _ = entry.delete_credential();
    }
}

pub fn connect(path: &PathBuf, name: &str) -> Result<()> {
    let servers = store::load(path)?;
    let idx = servers
        .iter()
        .position(|s| s.name == name)
        .with_context(|| {
            format!(
                "no server named '{name}' (see 'lssh ls')",
                name = name.red()
            )
        })?;

    println!(
        "{} connecting to {}@{} ...",
        "→".cyan().bold(),
        servers[idx].user.yellow(),
        servers[idx].host.blue()
    );

    // Ask for the password if none is saved.
    let mut password = get_saved_password(name);
    if password.is_none() {
        if !sshpass_available() {
            println!(
                "{} {} not installed, falling back to plain ssh (password will not be saved)",
                "ℹ".cyan().bold(),
                "sshpass".yellow()
            );
        } else {
            let pw = rpassword::prompt_password(format!(
                "enter password for {}@{}: ",
                servers[idx].user, servers[idx].host
            ))
            .context("failed to read password")?;
            if pw.is_empty() {
                bail!("empty password, aborting");
            }
            save_password(name, &pw);
            password = Some(pw);
        }
    }

    let target = format!("{}@{}", servers[idx].user, servers[idx].host);
    let status = match &password {
        Some(pw) => {
            if !sshpass_available() {
                bail!("{SSPASS_HINT}");
            }
            // Read the password from the env instead of argv so it never
            // shows up in the process list.
            std::process::Command::new("sshpass")
                .arg("-e")
                .arg("ssh")
                .arg(&target)
                .env("SSHPASS", pw)
                .status()
                .context("failed to launch sshpass (is openssh installed?)")?
        }
        None => std::process::Command::new("ssh")
            .arg(&target)
            .status()
            .context("failed to launch ssh (is openssh installed?)")?,
    };

    if !status.success() {
        bail!("ssh exited with {}", status);
    }
    Ok(())
}

pub fn remove(path: &PathBuf, name: &str) -> Result<()> {
    let mut servers = store::load(path)?;
    let original_len = servers.len();
    servers.retain(|s| s.name != name);

    if servers.len() == original_len {
        bail!("no server named '{name}' (see 'lssh ls')");
    }
    store::save(path, &servers)?;
    delete_saved_password(name);
    println!("{} removed server '{}'", "✔".green().bold(), name.cyan());
    Ok(())
}
