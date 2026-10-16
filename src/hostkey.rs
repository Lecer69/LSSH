//! First-connection host key handling.
//!
//! `sshpass` cannot answer ssh's "Are you sure you want to continue
//! connecting?" prompt: it exits with status 6 instead, which looked like the
//! session dropping right after connecting. Before handing a session to
//! sshpass, make sure the host key is already known, letting ssh ask the
//! user on the real terminal if it is not.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Ensure `target`'s host key is in known_hosts, prompting the user via ssh's
/// own fingerprint confirmation if it is not.
pub fn ensure_known(target: &str) -> Result<()> {
    if is_known(target) {
        return Ok(());
    }

    // Connect without trying any authentication: ssh verifies (and, on the
    // user's "yes", records) the host key, then fails with "Permission
    // denied", which is expected. The prompt itself goes to /dev/tty.
    let output = Command::new("ssh")
        .args([
            "-o",
            "PreferredAuthentications=none",
            "-o",
            "BatchMode=no",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "LogLevel=ERROR",
        ])
        .arg(target)
        .arg("true")
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("failed to launch ssh (is openssh installed?)")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("Host key verification failed") {
        let details = stderr.trim();
        bail!("host key for {target} was not accepted\n{details}");
    }
    if stderr.contains("Could not resolve hostname")
        || stderr.contains("Connection refused")
        || stderr.contains("Connection timed out")
        || stderr.contains("No route to host")
    {
        bail!("could not reach {target}: {}", stderr.trim());
    }
    Ok(())
}

/// Best-effort local check whether ssh already trusts `target`'s host key.
///
/// A false negative only costs an extra (silent) handshake in
/// [`ensure_known`], so anything unusual simply returns `false`.
fn is_known(target: &str) -> bool {
    let Ok(output) = Command::new("ssh")
        .arg("-G")
        .arg(target)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let resolved = String::from_utf8_lossy(&output.stdout);
    let Some(lookup) = lookup_name(&resolved) else {
        return false;
    };

    known_hosts_files(&resolved)
        .into_iter()
        .filter(|file| file.is_file())
        .any(|file| {
            Command::new("ssh-keygen")
                .arg("-F")
                .arg(&lookup)
                .arg("-f")
                .arg(&file)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
}

/// The name ssh looks up in known_hosts, from `ssh -G` output: the
/// HostKeyAlias if set, else `host` or `[host]:port` for non-default ports.
fn lookup_name(resolved: &str) -> Option<String> {
    let mut host = None;
    let mut port = 22u16;
    let mut alias = None;
    for line in resolved.lines() {
        match line.split_once(' ') {
            Some(("hostname", v)) => host = Some(v.trim()),
            Some(("port", v)) => port = v.trim().parse().unwrap_or(22),
            Some(("hostkeyalias", v)) => alias = Some(v.trim()),
            _ => {}
        }
    }
    if let Some(alias) = alias.filter(|a| !a.is_empty() && *a != "none") {
        return Some(alias.to_string());
    }
    let host = host.filter(|h| !h.is_empty())?;
    Some(if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    })
}

fn known_hosts_files(resolved: &str) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolved
        .lines()
        .filter_map(|line| {
            line.strip_prefix("userknownhostsfile ")
                .or_else(|| line.strip_prefix("globalknownhostsfile "))
        })
        .flat_map(str::split_whitespace)
        .filter_map(|f| match f.strip_prefix("~/") {
            Some(rest) => home.as_ref().map(|h| h.join(rest)),
            None => Some(PathBuf::from(f)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_uses_port_brackets() {
        assert_eq!(
            lookup_name("user root\nhostname 10.0.0.1\nport 22\n").as_deref(),
            Some("10.0.0.1")
        );
        assert_eq!(
            lookup_name("hostname example.com\nport 2222\n").as_deref(),
            Some("[example.com]:2222")
        );
        assert_eq!(
            lookup_name("hostname example.com\nport 2222\nhostkeyalias box\n").as_deref(),
            Some("box")
        );
        assert_eq!(lookup_name("port 22\n"), None);
    }

    #[test]
    fn known_hosts_files_are_split_and_expanded() {
        let files = known_hosts_files(
            "userknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2\n\
             globalknownhostsfile /etc/ssh/ssh_known_hosts\n",
        );
        assert_eq!(files.len(), 3);
        assert!(files[0].ends_with(".ssh/known_hosts"));
        assert_eq!(files[2], PathBuf::from("/etc/ssh/ssh_known_hosts"));
    }
}
