//! Commands that work with servers without an interactive session:
//! `run`, `cp`, `copy-id` and `status`.

use std::io::{BufRead, BufReader, ErrorKind, IsTerminal, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use colored::{Color, Colorize};

use crate::askpass::Askpass;
use crate::out::{self, Exit};
use crate::password;
use crate::store::{self, Server, Store};

/// Colours that tell servers apart in `run` output.
const LABEL_COLORS: [Color; 6] = [
    Color::Cyan,
    Color::Magenta,
    Color::Yellow,
    Color::Green,
    Color::Blue,
    Color::BrightRed,
];

fn ssh(server: &Server, askpass: &Askpass) -> Command {
    let mut command = Command::new("ssh");
    askpass.apply(&mut command);
    command
        .args(["-o", "ConnectTimeout=10"])
        .args(server.ssh_options());
    command
}

pub fn run(servers: &str, command: &[String]) -> Result<()> {
    let servers = Store::open()?.select(servers)?;
    let remote = command.join(" ");
    match servers.as_slice() {
        [server] => run_one(server, &remote),
        _ => run_many(&servers, &remote),
    }
}

/// Run on one server with the terminal attached; its exit status is ours.
fn run_one(server: &Server, remote: &str) -> Result<()> {
    let askpass = Askpass::new(server, true)?;
    let mut command = ssh(server, &askpass);
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        // A tty lets interactive commands (top, sudo, ...) work.
        command.arg("-t");
    }
    let status = command
        .arg(server.target())
        .arg(remote)
        .status()
        .context("failed to launch ssh (is openssh installed?)")?;
    match status.code() {
        Some(0) => Ok(()),
        Some(255) => bail!("ssh could not connect to {}", server.address()),
        Some(code) => Err(Exit(code.clamp(1, 255) as u8).into()),
        None => bail!("ssh was killed by a signal"),
    }
}

/// Run on several servers at once, prefixing every output line.
fn run_many(servers: &[Server], remote: &str) -> Result<()> {
    let width = servers
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0);

    let workers: Vec<_> = servers
        .iter()
        .enumerate()
        .map(|(i, server)| {
            let server = server.clone();
            let remote = remote.to_string();
            let pad = " ".repeat(width - server.name.chars().count());
            let label = format!("{}{pad} │", server.name)
                .color(LABEL_COLORS[i % LABEL_COLORS.len()])
                .to_string();
            thread::spawn(move || -> Result<Finished> {
                // Prompts from several servers at once would garble the
                // terminal, so only saved passwords and keys are used.
                let askpass = Askpass::new(&server, false)?;
                let mut child = ssh(&server, &askpass)
                    .arg(server.target())
                    .arg(&remote)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .context("failed to launch ssh (is openssh installed?)")?;
                let stdout = child.stdout.take().map(|out| {
                    let label = label.clone();
                    thread::spawn(move || prefix_lines(out, &label, false))
                });
                let stderr = child
                    .stderr
                    .take()
                    .map(|err| thread::spawn(move || prefix_lines(err, &label, true)));
                let mut denied = false;
                for reader in [stdout, stderr].into_iter().flatten() {
                    denied |= reader.join().unwrap_or(false);
                }
                Ok(Finished {
                    code: child.wait()?.code(),
                    denied,
                })
            })
        })
        .collect();

    let mut failed = Vec::new();
    let mut needs_login = false;
    for (server, worker) in servers.iter().zip(workers) {
        let outcome = match worker.join() {
            Ok(Ok(finished)) => match finished.code {
                Some(0) => continue,
                _ if finished.denied => {
                    needs_login = true;
                    "no saved password or key".to_string()
                }
                Some(255) => "could not connect".to_string(),
                Some(code) => format!("exit status {code}"),
                None => "killed by a signal".to_string(),
            },
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "crashed".to_string(),
        };
        failed.push((server, outcome));
    }

    println!();
    if failed.is_empty() {
        out::success(format_args!("succeeded on all {} servers", servers.len()));
        return Ok(());
    }
    for (server, outcome) in &failed {
        eprintln!("{} {}: {outcome}", "✖".red().bold(), out::name(server));
    }
    if needs_login {
        out::hint("connect once with 'lssh <name>' to save a password");
    }
    Err(Exit(1).into())
}

/// How one server's command ended.
struct Finished {
    code: Option<i32>,
    /// ssh said "Permission denied": no usable password or key.
    denied: bool,
}

/// Print each line of `stream` behind `label`. Returns whether ssh
/// reported a failed login.
fn prefix_lines(stream: impl Read, label: &str, stderr: bool) -> bool {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    let mut denied = false;
    while matches!(reader.read_until(b'\n', &mut line), Ok(n) if n > 0) {
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\n', '\r']);
        if stderr {
            denied |= text.contains("Permission denied");
            eprintln!("{label} {text}");
        } else {
            println!("{label} {text}");
        }
        line.clear();
    }
    denied
}

pub fn cp(from: &str, to: &str, recursive: bool) -> Result<()> {
    let store = Store::open()?;
    let (server, source, destination) = match (on_server(&store, from), on_server(&store, to)) {
        (Some((server, path)), None) => (server, server.scp_path(path), to.to_string()),
        (None, Some((server, path))) => (server, from.to_string(), server.scp_path(path)),
        (Some(_), Some(_)) => {
            bail!("copying between two servers isn't supported\ncopy to this machine first")
        }
        (None, None) => bail!(
            "neither '{from}' nor '{to}' is on a saved server\n\
             write remote paths as <server>:<path>, e.g. web:/etc/hosts"
        ),
    };

    let askpass = Askpass::new(server, true)?;
    let mut command = Command::new("scp");
    askpass.apply(&mut command);
    if recursive {
        command.arg("-r");
    }
    let status = command
        .args(server.ssh_options())
        .arg(&source)
        .arg(&destination)
        .status()
        .context("failed to launch scp (is openssh installed?)")?;
    if !status.success() {
        bail!("scp failed ({status})");
    }
    out::success(format_args!("copied {from} → {to}"));
    Ok(())
}

/// Split `<server>:<path>` when `<server>` is a saved server.
fn on_server<'a>(store: &'a Store, spec: &'a str) -> Option<(&'a Server, &'a str)> {
    let (name, path) = spec.split_once(':')?;
    Some((store.get(name).ok()?, path))
}

pub fn copy_id(name: &str, key: Option<&str>, keep_password: bool) -> Result<()> {
    let mut store = Store::open()?;
    let server = store.get(name)?.clone();
    let private = match (key, &server.identity) {
        (Some(key), _) => store::expand_path(key)?,
        (None, Some(identity)) => identity.clone(),
        (None, None) => newest_key()?,
    };
    let private = PathBuf::from(private.to_string_lossy().trim_end_matches(".pub"));
    let public = PathBuf::from(format!("{}.pub", private.display()));
    if !public.is_file() {
        bail!(
            "no public key at {}\ncreate a key pair with: ssh-keygen -t ed25519",
            public.display()
        );
    }

    out::step(format_args!(
        "installing {} on {}",
        public.display(),
        out::name(&server)
    ));
    let askpass = Askpass::new(&server, true)?;
    let mut command = Command::new("ssh-copy-id");
    askpass.apply(&mut command);
    let status = command
        .arg("-i")
        .arg(&public)
        .args(port_option(&server))
        .arg(server.target())
        .status()
        .context("failed to launch ssh-copy-id (is openssh installed?)")?;
    if !status.success() {
        bail!("ssh-copy-id failed ({status})");
    }

    // Only give up the password once the key is known to work.
    if !key_login_works(&server, &private) {
        out::warn("the key was installed, but logging in with it did not work");
        out::hint("the saved password is kept; a passphrase-protected key needs ssh-agent");
        return Ok(());
    }
    if !is_default_identity(&private) && server.identity.as_deref() != Some(private.as_path()) {
        store.set_identity(&server.name, private.clone())?;
        store.save()?;
    }
    if keep_password {
        out::success(format_args!(
            "{} now logs in with the key",
            out::name(&server)
        ));
    } else {
        password::delete(&server.name);
        out::success(format_args!(
            "{} now logs in with the key; the saved password was removed",
            out::name(&server)
        ));
    }
    Ok(())
}

fn port_option(server: &Server) -> Vec<String> {
    match server.port {
        Some(port) => vec!["-o".to_string(), format!("Port={port}")],
        None => Vec::new(),
    }
}

fn key_login_works(server: &Server, private: &Path) -> bool {
    Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "KbdInteractiveAuthentication=no",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "ConnectTimeout=10",
        ])
        .arg("-i")
        .arg(private)
        .args(port_option(server))
        .arg(server.target())
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Keys ssh tries on its own, so they need no `IdentityFile`.
fn is_default_identity(private: &Path) -> bool {
    let Ok(home) = store::home_dir() else {
        return false;
    };
    [
        "id_rsa",
        "id_ecdsa",
        "id_ecdsa_sk",
        "id_ed25519",
        "id_ed25519_sk",
    ]
    .iter()
    .any(|name| private == home.join(".ssh").join(name))
}

/// The most recently modified key pair in `~/.ssh`.
fn newest_key() -> Result<PathBuf> {
    let dir = store::home_dir()?.join(".ssh");
    let newest = std::fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "pub"))
        .filter(|public| public.with_extension("").is_file())
        .max_by_key(|public| public.metadata().and_then(|m| m.modified()).ok());
    match newest {
        Some(public) => Ok(public.with_extension("")),
        None => bail!(
            "no SSH key found in {}\ncreate one with: ssh-keygen -t ed25519",
            dir.display()
        ),
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// A reachable SSH port.
struct Probe {
    latency: Duration,
    /// The server's identification, e.g. `OpenSSH_9.6p1 Ubuntu-3ubuntu13`.
    banner: Option<String>,
}

pub fn status(servers: Option<&str>) -> Result<()> {
    let servers = Store::open()?.select(servers.unwrap_or("all"))?;
    let probes: Vec<_> = servers
        .iter()
        .cloned()
        .map(|server| thread::spawn(move || probe(&server)))
        .collect();

    let width = servers
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut down = 0;
    for (server, probe) in servers.iter().zip(probes) {
        let pad = " ".repeat(width - server.name.chars().count());
        let result = probe
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("probe crashed")));
        match result {
            Ok(probe) => println!(
                "  {}{pad}   {} {}   {:>6}   {}",
                out::name(server),
                "●".green(),
                "up  ".green(),
                format!("{} ms", probe.latency.as_millis()),
                probe.banner.unwrap_or_default().dimmed()
            ),
            Err(e) => {
                down += 1;
                println!(
                    "  {}{pad}   {} {}   {}",
                    out::name(server),
                    "●".red(),
                    "down".red(),
                    format!("{e:#}").dimmed()
                );
            }
        }
    }
    if down > 0 {
        return Err(Exit(1).into());
    }
    Ok(())
}

fn probe(server: &Server) -> Result<Probe> {
    let addrs: Vec<SocketAddr> = (server.host.as_str(), server.port_or_default())
        .to_socket_addrs()
        .context("could not resolve host")?
        .collect();
    let mut last_error = None;
    for addr in addrs {
        let start = Instant::now();
        match TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) {
            Ok(stream) => {
                let latency = start.elapsed();
                return Ok(Probe {
                    latency,
                    banner: read_banner(stream),
                });
            }
            Err(e) => last_error = Some(e),
        }
    }
    let reason = match last_error {
        None => "host has no address".to_string(),
        Some(e) => match e.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => "timed out".to_string(),
            ErrorKind::ConnectionRefused => "connection refused".to_string(),
            ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable => "unreachable".to_string(),
            _ => e.to_string(),
        },
    };
    bail!("{reason}")
}

/// The first line an SSH server sends: `SSH-2.0-<software>`.
fn read_banner(stream: TcpStream) -> Option<String> {
    stream.set_read_timeout(Some(PROBE_TIMEOUT)).ok()?;
    let mut line = Vec::new();
    BufReader::new(stream.take(255))
        .read_until(b'\n', &mut line)
        .ok()?;
    let line = String::from_utf8_lossy(&line);
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    Some(
        line.strip_prefix("SSH-2.0-")
            .or_else(|| line.strip_prefix("SSH-1.99-"))
            .unwrap_or(line)
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn probe_reads_the_ssh_banner() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_thread = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(b"SSH-2.0-OpenSSH_9.6\r\n").unwrap();
        });
        let server = Server::new("local", &format!("me@127.0.0.1:{port}")).unwrap();
        let probe = probe(&server).unwrap();
        assert_eq!(probe.banner.as_deref(), Some("OpenSSH_9.6"));
        server_thread.join().unwrap();
    }

    #[test]
    fn probe_reports_refused_connections() {
        // Bind and drop to find a port nothing listens on.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let server = Server::new("local", &format!("me@127.0.0.1:{port}")).unwrap();
        let error = probe(&server).err().unwrap();
        assert_eq!(error.to_string(), "connection refused");
    }

    #[test]
    fn remote_paths_need_a_saved_server() {
        let path = std::env::temp_dir().join(format!("lssh-test-cp-{}.json", std::process::id()));
        let mut store = Store::open_at(path).unwrap();
        store
            .add(Server::new("web", "root@10.0.0.1").unwrap())
            .unwrap();
        let (server, rest) = on_server(&store, "web:/etc/hosts").unwrap();
        assert_eq!((server.name.as_str(), rest), ("web", "/etc/hosts"));
        assert!(on_server(&store, "db:/etc/hosts").is_none());
        assert!(on_server(&store, "./local").is_none());
    }
}
