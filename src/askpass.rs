//! lssh as its own `SSH_ASKPASS` helper (this replaces sshpass).
//!
//! ssh runs `$SSH_ASKPASS "<prompt>"` whenever it needs to ask something and
//! reads the answer from the helper's stdout. With `SSH_ASKPASS_REQUIRE=force`
//! (OpenSSH 8.4+) it does that for every prompt, even on a terminal. Pointing
//! it at the lssh binary lets lssh answer password prompts from the OS
//! keyring, and show everything else (host key confirmation, key
//! passphrases, 2FA codes) on the terminal, just like ssh itself would.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};

use crate::password;
use crate::store::{self, Server};

/// Saved server whose password is being asked for.
const SERVER_VAR: &str = "LSSH_ASKPASS_SERVER";
/// Directory where attempts are tracked (see [`first_attempt`]).
const STATE_VAR: &str = "LSSH_ASKPASS_STATE";
/// "0" when nothing may be asked on the terminal.
const INTERACTIVE_VAR: &str = "LSSH_ASKPASS_INTERACTIVE";

/// The environment that makes ssh (and scp, ssh-copy-id, ...) ask lssh.
/// Its state directory is removed on drop.
pub struct Askpass {
    vars: Vec<(&'static str, String)>,
    state: PathBuf,
}

impl Askpass {
    /// `interactive` allows prompting on the terminal when there is no
    /// saved password (or it was rejected). Turn it off when several
    /// connections run at once.
    pub fn new(server: &Server, interactive: bool) -> Result<Askpass> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let exe = std::env::current_exe().context("could not locate the lssh binary")?;
        let state = store::runtime_dir().join(format!(
            "lssh-askpass-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        store::create_private_dir(&state)?;

        let vars = vec![
            ("SSH_ASKPASS", exe.display().to_string()),
            ("SSH_ASKPASS_REQUIRE", "force".to_string()),
            (SERVER_VAR, server.name.clone()),
            (STATE_VAR, state.display().to_string()),
            (
                INTERACTIVE_VAR,
                if interactive { "1" } else { "0" }.to_string(),
            ),
        ];
        Ok(Askpass { vars, state })
    }

    pub fn vars(&self) -> &[(&'static str, String)] {
        &self.vars
    }

    pub fn apply<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command.envs(self.vars.iter().map(|(k, v)| (k, v)))
    }
}

impl Drop for Askpass {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

/// Whether ssh started this process as its askpass helper.
pub fn invoked() -> bool {
    std::env::var_os(SERVER_VAR).is_some() && std::env::var_os("SSH_ASKPASS").is_some()
}

/// Entry point in askpass mode: answer the prompt in `argv[1]` on stdout.
pub fn main() -> ExitCode {
    let prompt = std::env::args().nth(1).unwrap_or_default();
    match answer(&prompt) {
        Ok(reply) => {
            println!("{reply}");
            ExitCode::SUCCESS
        }
        Err(_) => ExitCode::FAILURE,
    }
}

#[derive(Debug, PartialEq)]
enum Kind {
    /// The server's login password: answered from the keyring.
    Password,
    /// A yes/no question, e.g. accepting a new host key.
    Confirm,
    /// Anything else secret: key passphrases, PINs, one-time codes.
    Secret,
}

fn classify(prompt: &str) -> Kind {
    let lower = prompt.to_lowercase();
    if lower.contains("yes/no") || std::env::var("SSH_ASKPASS_PROMPT").as_deref() == Ok("confirm") {
        Kind::Confirm
    } else if lower.contains("passphrase") {
        Kind::Secret
    } else if lower.contains("password") {
        Kind::Password
    } else {
        Kind::Secret
    }
}

fn answer(prompt: &str) -> Result<String> {
    let server = std::env::var(SERVER_VAR).context("not running as askpass")?;
    let interactive = std::env::var(INTERACTIVE_VAR).as_deref() != Ok("0");
    match classify(prompt) {
        Kind::Password => server_password(&server, prompt, interactive),
        _ if !interactive => bail!("can't ask on the terminal"),
        Kind::Confirm => ask_line(prompt),
        Kind::Secret => Ok(rpassword::prompt_password(prompt)?),
    }
}

/// The saved password on ssh's first try; on a retry it was rejected, so
/// ask for the right one and remember it instead.
fn server_password(server: &str, prompt: &str, interactive: bool) -> Result<String> {
    let first = first_attempt()?;
    if first && let Ok(Some(saved)) = password::get(server) {
        return Ok(saved);
    }
    if !interactive {
        bail!("no usable saved password");
    }
    let prompt = if first {
        prompt.to_string()
    } else {
        format!("Permission denied, please try again.\n{prompt}")
    };
    let entered = rpassword::prompt_password(prompt)?;
    if !entered.is_empty() {
        // Best effort: a broken keyring must not block the login.
        let _ = password::set(server, &entered);
    }
    Ok(entered)
}

/// True the first time the calling ssh process asks for a password.
///
/// Each askpass call is a fresh process, so attempts are tracked per parent
/// (ssh) pid with a marker file. One lssh command can run ssh several times
/// (ssh-copy-id does), and each of those gets its own first try.
fn first_attempt() -> Result<bool> {
    let state = std::env::var_os(STATE_VAR).context("missing askpass state")?;
    let marker = PathBuf::from(state).join(std::os::unix::process::parent_id().to_string());
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).context("failed to track password attempts"),
    }
}

/// Ask on the terminal with echo, e.g. for the host key yes/no question.
fn ask_line(prompt: &str) -> Result<String> {
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("no terminal to ask on")?;
    tty.write_all(prompt.as_bytes())?;
    tty.flush()?;
    let mut line = String::new();
    BufReader::new(tty).read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_ssh_prompts() {
        assert_eq!(classify("root@10.0.0.1's password: "), Kind::Password);
        assert_eq!(classify("(root@10.0.0.1) Password: "), Kind::Password);
        assert_eq!(
            classify("Enter passphrase for key '/home/me/.ssh/id_ed25519': "),
            Kind::Secret
        );
        assert_eq!(
            classify(
                "The authenticity of host '10.0.0.1 (10.0.0.1)' can't be established.\n\
                 ED25519 key fingerprint is SHA256:abc.\n\
                 Are you sure you want to continue connecting (yes/no/[fingerprint])? "
            ),
            Kind::Confirm
        );
        assert_eq!(classify("Verification code: "), Kind::Secret);
    }

    #[test]
    fn askpass_environment_is_complete_and_cleaned_up() {
        let server = Server::new("web", "root@host").unwrap();
        let askpass = Askpass::new(&server, false).unwrap();
        let get = |key: &str| {
            askpass
                .vars()
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("SSH_ASKPASS_REQUIRE").as_deref(), Some("force"));
        assert_eq!(get(SERVER_VAR).as_deref(), Some("web"));
        assert_eq!(get(INTERACTIVE_VAR).as_deref(), Some("0"));
        let state = PathBuf::from(get(STATE_VAR).unwrap());
        assert!(state.is_dir());
        drop(askpass);
        assert!(!state.exists());
    }
}
