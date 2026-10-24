//! Reaching servers through a SOCKS5 or HTTP proxy (`-x`).
//!
//! ssh only uses a proxy through a `ProxyCommand`, usually netcat
//! (`nc -X 5 -x 127.0.0.1:10808 %h %p`). lssh is its own instead, the same
//! way it is its own askpass helper: ssh runs
//! `lssh --proxy-connect <proxy> <host> <port>`, which asks the proxy for a
//! tunnel to the server and relays ssh's stdin and stdout through it.
//! `lssh status` probes servers through the same kind of tunnel.

use std::fmt;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::config;
use crate::out;
use crate::store;

/// First argument of lssh as a `ProxyCommand`. It starts with a dash, so
/// no server can have this name.
const HELPER_ARG: &str = "--proxy-connect";

/// How long the helper waits for the proxy, and for the proxy to reach
/// the server.
const HELPER_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Socks5,
    Http,
}

/// A proxy server, as saved with `lssh proxy set`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    kind: Kind,
    host: String,
    port: u16,
}

impl Proxy {
    /// Parse `host:port`, `socks5://host:port` or `http://host:port`.
    pub fn parse(text: &str) -> Result<Proxy> {
        let text = text.trim();
        let invalid = || {
            anyhow!(
                "invalid proxy '{text}'\n\
                 expected host:port, socks5://host:port or http://host:port"
            )
        };
        let (kind, rest) = match text.split_once("://") {
            None => (Kind::Socks5, text),
            Some((scheme, rest)) => {
                let kind = match scheme.to_ascii_lowercase().as_str() {
                    // socks5h means "the proxy resolves names", which lssh
                    // always lets it do.
                    "socks5" | "socks5h" => Kind::Socks5,
                    "http" => Kind::Http,
                    _ => bail!(
                        "unsupported proxy type '{scheme}'\n\
                         lssh talks to socks5:// and http:// proxies"
                    ),
                };
                (kind, rest)
            }
        };
        if rest.contains('@') {
            bail!("proxies that need a login (user:password@) are not supported");
        }
        let (host, port) =
            store::split_port(rest.strip_suffix('/').unwrap_or(rest)).map_err(|_| invalid())?;
        let valid_host = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%'));
        match port {
            Some(port) if valid_host => Ok(Proxy { kind, host, port }),
            _ => Err(invalid()),
        }
    }

    /// `host:port`, with IPv6 hosts in brackets.
    fn address(&self) -> String {
        format!("{}:{}", store::bracket_ipv6(&self.host), self.port)
    }

    /// Open a connection to `host:port` through the proxy. Host names are
    /// left for the proxy to resolve, since the local DNS may be part of
    /// what it routes around.
    pub fn connect(&self, host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
        let mut stream = self.dial(timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        match self.kind {
            Kind::Socks5 => self.socks5(&mut stream, host, port)?,
            Kind::Http => self.http_connect(&mut stream, host, port)?,
        }
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;
        Ok(stream)
    }

    fn dial(&self, timeout: Duration) -> Result<TcpStream> {
        let unreachable =
            |reason: String| anyhow!("could not reach the proxy at {}: {reason}", self.address());
        let addrs: Vec<SocketAddr> = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|_| unreachable("could not resolve its host".to_string()))?
            .collect();
        let mut last_error = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, timeout) {
                Ok(stream) => return Ok(stream),
                Err(e) => last_error = Some(e),
            }
        }
        Err(unreachable(match last_error {
            Some(e) => out::connect_error(&e),
            None => "its host has no address".to_string(),
        }))
    }

    /// SOCKS5 (RFC 1928) without authentication.
    fn socks5(&self, stream: &mut TcpStream, host: &str, port: u16) -> Result<()> {
        // Version 5, one authentication method: none.
        send(stream, &[5, 1, 0])?;
        let mut choice = [0u8; 2];
        receive(stream, &mut choice)?;
        match choice {
            [5, 0] => {}
            [5, 0xff] => {
                bail!("the proxy wants a username and password, which lssh does not support")
            }
            _ => bail!("{} did not answer like a SOCKS5 proxy", self.address()),
        }

        // CONNECT: version, command, reserved, address type, address, port.
        let mut request = vec![5, 1, 0];
        match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) => {
                request.push(1);
                request.extend(ip.octets());
            }
            Ok(IpAddr::V6(ip)) => {
                request.push(4);
                request.extend(ip.octets());
            }
            Err(_) => {
                let len = u8::try_from(host.len())
                    .with_context(|| format!("host name '{host}' is too long"))?;
                request.extend([3, len]);
                request.extend(host.as_bytes());
            }
        }
        request.extend(port.to_be_bytes());
        send(stream, &request)?;

        // Version, reply code, reserved, then the address the proxy bound.
        let mut reply = [0u8; 4];
        receive(stream, &mut reply)?;
        if reply[0] != 5 {
            bail!("{} did not answer like a SOCKS5 proxy", self.address());
        }
        if reply[1] != 0 {
            bail!(
                "the proxy could not reach {}: {}",
                target(host, port),
                socks5_error(reply[1])
            );
        }
        let bound = match reply[3] {
            1 => 4,
            4 => 16,
            3 => {
                let mut len = [0u8; 1];
                receive(stream, &mut len)?;
                usize::from(len[0])
            }
            other => bail!("the proxy answered with an unknown address type ({other})"),
        };
        let mut rest = vec![0u8; bound + 2];
        receive(stream, &mut rest)
    }

    /// An HTTP `CONNECT` tunnel.
    fn http_connect(&self, stream: &mut TcpStream, host: &str, port: u16) -> Result<()> {
        let target = target(host, port);
        send(
            stream,
            format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes(),
        )?;

        // Read byte by byte: the server's first words (its SSH banner) may
        // come right behind the reply, and they are for ssh.
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") && !head.ends_with(b"\n\n") {
            if head.len() > 16 * 1024 {
                bail!("{} sent an endless reply", self.address());
            }
            let mut byte = [0u8; 1];
            receive(stream, &mut byte)?;
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head);
        let status = head.lines().next().unwrap_or_default().trim();
        let mut words = status.split_whitespace();
        if !words.next().is_some_and(|word| word.starts_with("HTTP/")) {
            bail!("{} did not answer like an HTTP proxy", self.address());
        }
        if !words.next().is_some_and(|code| code.starts_with('2')) {
            bail!("the proxy refused the tunnel to {target}: {status}");
        }
        Ok(())
    }
}

impl fmt::Display for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = match self.kind {
            Kind::Socks5 => "socks5",
            Kind::Http => "http",
        };
        write!(f, "{scheme}://{}", self.address())
    }
}

/// `host:port` of the server, as a proxy would write it.
fn target(host: &str, port: u16) -> String {
    format!("{}:{port}", store::bracket_ipv6(host))
}

fn send(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    stream.write_all(data).map_err(handshake_error)
}

fn receive(stream: &mut TcpStream, buf: &mut [u8]) -> Result<()> {
    stream.read_exact(buf).map_err(handshake_error)
}

/// An I/O error while setting up the tunnel, as people would put it.
fn handshake_error(e: io::Error) -> anyhow::Error {
    match e.kind() {
        ErrorKind::UnexpectedEof => anyhow!("the proxy closed the connection"),
        ErrorKind::TimedOut | ErrorKind::WouldBlock => anyhow!("the proxy did not answer in time"),
        _ => anyhow!("the connection to the proxy failed: {e}"),
    }
}

/// A SOCKS5 reply code, in words.
fn socks5_error(code: u8) -> &'static str {
    match code {
        1 => "general failure",
        2 => "not allowed by the proxy's rules",
        3 => "network unreachable",
        4 => "host unreachable",
        5 => "connection refused",
        6 => "timed out",
        7 => "command not supported",
        8 => "address type not supported",
        _ => "unknown error",
    }
}

/// The proxy saved with `lssh proxy set`, when a command was given `-x`.
pub fn resolve(wanted: bool) -> Result<Option<Proxy>> {
    if !wanted {
        return Ok(None);
    }
    match config::load().proxy {
        Some(proxy) => Proxy::parse(&proxy).map(Some),
        None => bail!("no proxy set\nset one with: lssh proxy set <host:port>"),
    }
}

/// The `-o ProxyCommand=...` that sends ssh, scp or ssh-copy-id through
/// `proxy`, with lssh as the command; nothing without a proxy.
pub fn ssh_options(proxy: Option<&Proxy>) -> Result<Vec<String>> {
    let Some(proxy) = proxy else {
        return Ok(Vec::new());
    };
    let exe = std::env::current_exe().context("could not locate the lssh binary")?;
    Ok(vec![
        "-o".to_string(),
        format!("ProxyCommand={}", proxy_command(&exe, proxy)),
    ])
}

/// ssh fills in `%h` and `%p`, then runs this with `$SHELL -c`.
fn proxy_command(exe: &Path, proxy: &Proxy) -> String {
    format!(
        "{} {HELPER_ARG} {} %h %p",
        shell_word(&exe.to_string_lossy()),
        shell_word(&proxy.to_string())
    )
}

/// Quote one word for the shell. `%` is doubled because ssh expands `%`
/// tokens before the shell sees the command.
fn shell_word(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''").replace('%', "%%"))
}

/// Whether ssh started this process as its `ProxyCommand`.
pub fn invoked() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == HELPER_ARG)
}

/// Entry point as a `ProxyCommand`: `--proxy-connect <proxy> <host> <port>`.
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(2).collect();
    match relay(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // ssh passes this on to the terminal, before its own complaint.
            eprintln!("lssh: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Open the tunnel, then shuttle bytes between ssh and the server until
/// the server hangs up.
fn relay(args: &[String]) -> Result<()> {
    let [proxy, host, port] = args else {
        bail!("usage: lssh {HELPER_ARG} <proxy> <host> <port>");
    };
    let port: u16 = port
        .parse()
        .with_context(|| format!("invalid port '{port}'"))?;
    let stream = Proxy::parse(proxy)?.connect(host, port, HELPER_TIMEOUT)?;
    // Keystrokes should leave right away, as they do with plain ssh.
    let _ = stream.set_nodelay(true);

    let mut upload = stream
        .try_clone()
        .context("failed to share the connection")?;
    thread::spawn(move || {
        let _ = pump(io::stdin().lock(), &mut upload);
        // ssh is done sending; the server's answer may still be on its way.
        let _ = upload.shutdown(Shutdown::Write);
    });
    pump(&stream, io::stdout().lock()).context("the connection to the server broke")
}

/// Copy until end of file, flushing each chunk: stdout would otherwise
/// hold back data without a newline in it.
fn pump(mut from: impl Read, mut to: impl Write) -> io::Result<()> {
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&buf[..n])?;
        to.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::process::Command;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn parsed(text: &str) -> String {
        Proxy::parse(text).unwrap().to_string()
    }

    #[test]
    fn parses_proxy_addresses() {
        assert_eq!(parsed("127.0.0.1:10808"), "socks5://127.0.0.1:10808");
        assert_eq!(
            parsed(" socks5://127.0.0.1:1080/ "),
            "socks5://127.0.0.1:1080"
        );
        assert_eq!(
            parsed("SOCKS5H://proxy.lan:1080"),
            "socks5://proxy.lan:1080"
        );
        assert_eq!(parsed("http://127.0.0.1:10809"), "http://127.0.0.1:10809");
        assert_eq!(parsed("[::1]:1080"), "socks5://[::1]:1080");
        for bad in [
            "",
            "127.0.0.1",
            "socks5://",
            ":1080",
            "::1",
            "host:0",
            "host:99999",
            "host:http",
            "a b:1080",
            "http://host:8080/path",
        ] {
            assert!(Proxy::parse(bad).is_err(), "{bad} should be rejected");
        }
        let error = |text: &str| Proxy::parse(text).unwrap_err().to_string();
        assert!(error("https://host:443").starts_with("unsupported proxy type 'https'"));
        assert!(error("socks4://host:1080").starts_with("unsupported proxy type"));
        assert!(error("socks5://me:secret@host:1080").contains("login"));
    }

    /// Expand ssh's `%` tokens the way ssh does, then let `sh` split the
    /// command into words.
    fn run_through_ssh_and_sh(command: &str, host: &str, port: &str) -> Vec<String> {
        let mut expanded = String::new();
        let mut chars = command.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                expanded.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => expanded.push('%'),
                Some('h') => expanded.push_str(host),
                Some('p') => expanded.push_str(port),
                other => panic!("ssh would reject %{other:?}"),
            }
        }
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\n' {expanded}"))
            .output()
            .unwrap();
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn proxy_command_survives_ssh_and_the_shell() {
        let proxy = Proxy::parse("127.0.0.1:10808").unwrap();
        assert_eq!(
            proxy_command(Path::new("/usr/bin/lssh"), &proxy),
            "'/usr/bin/lssh' --proxy-connect 'socks5://127.0.0.1:10808' %h %p"
        );
        let exe = "/opt/it's 100%/lssh";
        let command = proxy_command(Path::new(exe), &proxy);
        assert_eq!(
            run_through_ssh_and_sh(&command, "example.com", "22"),
            [
                exe,
                HELPER_ARG,
                "socks5://127.0.0.1:10808",
                "example.com",
                "22"
            ]
        );
        assert!(ssh_options(None).unwrap().is_empty());
    }

    /// A proxy on localhost that handles one connection with `serve`.
    fn fake_proxy(
        kind: &str,
        serve: impl FnOnce(TcpStream) + Send + 'static,
    ) -> (Proxy, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            serve(socket);
        });
        let proxy = Proxy::parse(&format!("{kind}://127.0.0.1:{port}")).unwrap();
        (proxy, handle)
    }

    fn read_n(socket: &mut TcpStream, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        socket.read_exact(&mut buf).unwrap();
        buf
    }

    fn first_line(stream: TcpStream) -> String {
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        line
    }

    #[test]
    fn socks5_leaves_host_names_to_the_proxy() {
        let (proxy, server) = fake_proxy("socks5", |mut socket| {
            assert_eq!(read_n(&mut socket, 3), [5, 1, 0]);
            socket.write_all(&[5, 0]).unwrap();
            let mut request = vec![5, 1, 0, 3, 11];
            request.extend(b"example.com");
            request.extend(2222u16.to_be_bytes());
            assert_eq!(read_n(&mut socket, request.len()), request);
            // Success, bound to a host name, then the server's banner.
            socket
                .write_all(&[5, 0, 0, 3, 2, b'h', b'i', 0, 0])
                .unwrap();
            socket.write_all(b"SSH-2.0-OpenSSH_9.6\r\n").unwrap();
        });
        let stream = proxy.connect("example.com", 2222, TIMEOUT).unwrap();
        assert_eq!(first_line(stream), "SSH-2.0-OpenSSH_9.6\r\n");
        server.join().unwrap();
    }

    #[test]
    fn socks5_sends_ip_addresses_as_addresses() {
        let mut ipv6 = vec![4];
        ipv6.extend(
            "2001:db8::1"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets(),
        );
        for (host, address) in [("10.0.0.1", vec![1, 10, 0, 0, 1]), ("2001:db8::1", ipv6)] {
            let (proxy, server) = fake_proxy("socks5", move |mut socket| {
                read_n(&mut socket, 3);
                socket.write_all(&[5, 0]).unwrap();
                let mut request = vec![5, 1, 0];
                request.extend(&address);
                request.extend(22u16.to_be_bytes());
                assert_eq!(read_n(&mut socket, request.len()), request);
                socket.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
            });
            proxy.connect(host, 22, TIMEOUT).unwrap();
            server.join().unwrap();
        }
    }

    #[test]
    fn socks5_failures_are_explained() {
        let (proxy, server) = fake_proxy("socks5", |mut socket| {
            read_n(&mut socket, 3);
            socket.write_all(&[5, 0]).unwrap();
            read_n(&mut socket, 10);
            socket.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
        });
        let error = proxy.connect("10.0.0.1", 22, TIMEOUT).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the proxy could not reach 10.0.0.1:22: connection refused"
        );
        server.join().unwrap();

        let (proxy, server) = fake_proxy("socks5", |mut socket| {
            read_n(&mut socket, 3);
            socket.write_all(&[5, 0xff]).unwrap();
        });
        let error = proxy.connect("10.0.0.1", 22, TIMEOUT).unwrap_err();
        assert!(error.to_string().contains("username and password"));
        server.join().unwrap();

        let (proxy, server) = fake_proxy("socks5", |mut socket| {
            read_n(&mut socket, 3);
        });
        let error = proxy.connect("10.0.0.1", 22, TIMEOUT).unwrap_err();
        assert_eq!(error.to_string(), "the proxy closed the connection");
        server.join().unwrap();
    }

    #[test]
    fn http_tunnel_keeps_what_follows_the_reply() {
        let (proxy, server) = fake_proxy("http", |mut socket| {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.extend(read_n(&mut socket, 1));
            }
            assert_eq!(
                String::from_utf8(request).unwrap(),
                "CONNECT [2001:db8::1]:2222 HTTP/1.1\r\nHost: [2001:db8::1]:2222\r\n\r\n"
            );
            // The reply and the banner in one go.
            socket
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\nSSH-2.0-OpenSSH_9.6\r\n")
                .unwrap();
        });
        let stream = proxy.connect("2001:db8::1", 2222, TIMEOUT).unwrap();
        assert_eq!(first_line(stream), "SSH-2.0-OpenSSH_9.6\r\n");
        server.join().unwrap();
    }

    #[test]
    fn http_refusals_show_the_status() {
        let (proxy, server) = fake_proxy("http", |mut socket| {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.extend(read_n(&mut socket, 1));
            }
            socket
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let error = proxy.connect("example.com", 22, TIMEOUT).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the proxy refused the tunnel to example.com:22: HTTP/1.1 403 Forbidden"
        );
        server.join().unwrap();
    }

    #[test]
    fn unreachable_proxies_are_named() {
        // Bind and drop to find a port nothing listens on.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let proxy = Proxy::parse(&format!("127.0.0.1:{port}")).unwrap();
        let error = proxy.connect("example.com", 22, TIMEOUT).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("could not reach the proxy at 127.0.0.1:{port}: connection refused")
        );
    }

    #[test]
    fn relay_checks_its_arguments() {
        assert!(relay(&[]).unwrap_err().to_string().starts_with("usage:"));
        let args = ["127.0.0.1:1080", "example.com", "ssh"].map(String::from);
        assert_eq!(relay(&args).unwrap_err().to_string(), "invalid port 'ssh'");
    }

    #[test]
    fn pump_copies_everything() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut copy = Vec::new();
        pump(&data[..], &mut copy).unwrap();
        assert_eq!(copy, data);
    }
}
