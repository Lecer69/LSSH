//! Saved passwords. They live in the OS keyring, never on disk.

use anyhow::{Context, Result, bail};

/// Keyring service name; entries are keyed by server name.
const SERVICE: &str = "lssh";

fn entry(name: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, name).context("could not access the OS keyring")
}

/// The saved password for server `name`, if there is one.
pub fn get(name: &str) -> Result<Option<String>> {
    match entry(name)?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("could not read from the OS keyring"),
    }
}

/// Save (or replace) the password for server `name`.
pub fn set(name: &str, password: &str) -> Result<()> {
    entry(name)?
        .set_password(password)
        .context("could not save to the OS keyring")
}

/// Forget the password for server `name`. A missing entry is fine.
pub fn delete(name: &str) {
    if let Ok(entry) = entry(name) {
        let _ = entry.delete_credential();
    }
}

/// Read a password from the terminal without echoing it.
pub fn prompt(label: &str) -> Result<String> {
    let password = rpassword::prompt_password(label).context("failed to read password")?;
    if password.is_empty() {
        bail!("empty password, aborting");
    }
    Ok(password)
}
