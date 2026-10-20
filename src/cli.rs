//! Command-line interface: the command table, parsing and help screens.
//!
//! Every command is described once in [`COMMANDS`]; the parser, both help
//! screens and the shell completions are generated from it, so they cannot
//! drift apart.

use std::fmt;

use anyhow::{Result, bail};
use colored::Colorize;

/// A parsed command line.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// Plain `lssh`: pick a server interactively.
    Pick,
    Add {
        name: String,
        target: String,
        key: Option<String>,
    },
    Import {
        file: Option<String>,
    },
    List {
        names_only: bool,
    },
    Rename {
        old: String,
        new: String,
    },
    Remove {
        name: String,
    },
    Passwd {
        name: String,
    },
    CopyId {
        name: String,
        key: Option<String>,
        keep_password: bool,
    },
    Connect {
        name: String,
        record: bool,
    },
    Run {
        servers: String,
        command: Vec<String>,
    },
    Cp {
        from: String,
        to: String,
        recursive: bool,
    },
    Status {
        servers: Option<String>,
    },
    Top {
        servers: Option<String>,
    },
    Config {
        action: Option<String>,
    },
    Completions {
        shell: String,
    },
    Help {
        topic: Option<String>,
    },
    Version,
}

/// Sections of the help overview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Servers,
    Remote,
    Setup,
}

impl Group {
    const ALL: [Group; 3] = [Group::Servers, Group::Remote, Group::Setup];

    fn title(self) -> &'static str {
        match self {
            Group::Servers => "Servers",
            Group::Remote => "Remote",
            Group::Setup => "Setup",
        }
    }
}

/// What a command's first argument completes to in the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Complete {
    Nothing,
    Server,
    /// A server, a comma-separated list of them, or `all`.
    Servers,
    /// `<server>:<path>` or a local path.
    RemotePath,
    File,
    Command,
    Choices(&'static [&'static str]),
}

/// A `--long` (and maybe `-s`) option.
pub struct Opt {
    pub long: &'static str,
    pub short: Option<char>,
    /// Placeholder of the option's value; `None` for flags.
    pub value: Option<&'static str>,
    pub about: &'static str,
}

impl Opt {
    fn matches(&self, word: &str) -> bool {
        word == self.long
            || self
                .short
                .is_some_and(|c| word.len() == 2 && word.starts_with('-') && word.ends_with(c))
    }

    /// `-k, --key <file>`
    fn signature(&self) -> String {
        let short = match self.short {
            Some(c) => format!("-{c}, "),
            None => "    ".to_string(),
        };
        match self.value {
            Some(value) => format!("{short}{} {value}", self.long),
            None => format!("{short}{}", self.long),
        }
    }
}

/// One entry of the command table.
pub struct Spec {
    pub name: &'static str,
    pub alias: Option<&'static str>,
    pub group: Group,
    /// Positional arguments. Optional ones are written `[like this]` and
    /// come last; a final `<like>...` takes all remaining words.
    pub args: &'static [&'static str],
    pub options: &'static [Opt],
    /// One-line summary for the command list.
    pub about: &'static str,
    /// Longer explanation for `lssh help <command>`.
    pub details: &'static str,
    pub examples: &'static [&'static str],
    pub completes: Complete,
    /// Turn the checked arguments into a [`Command`].
    build: fn(&Parsed) -> Command,
}

const KEY_OPT: Opt = Opt {
    long: "--key",
    short: Some('k'),
    value: Some("<file>"),
    about: "log in with this private key",
};

pub const COMMANDS: &[Spec] = &[
    Spec {
        name: "add",
        alias: None,
        group: Group::Servers,
        args: &["<name>", "<user@host[:port]>"],
        options: &[KEY_OPT],
        about: "save a new server",
        details: "The target may also be written host@user when the host is an IP address.\n\
                  IPv6 hosts need brackets when a port follows: root@[2001:db8::1]:2222.",
        examples: &[
            "lssh add web root@11.12.13.14",
            "lssh add db admin@db.example.com:2222 --key ~/.ssh/id_db",
        ],
        completes: Complete::Nothing,
        build: |p| Command::Add {
            name: p.arg(0),
            target: p.arg(1),
            key: p.value("--key"),
        },
    },
    Spec {
        name: "import",
        alias: None,
        group: Group::Servers,
        args: &["[file]"],
        options: &[],
        about: "import hosts from ~/.ssh/config",
        details: "Reads the Host entries (HostName, User, Port, IdentityFile) of an OpenSSH\n\
                  config file, ~/.ssh/config by default. Wildcard hosts, Match blocks and\n\
                  names that are already saved are skipped.",
        examples: &["lssh import", "lssh import ~/work/ssh_config"],
        completes: Complete::File,
        build: |p| Command::Import {
            file: p.optional(0),
        },
    },
    Spec {
        name: "list",
        alias: Some("ls"),
        group: Group::Servers,
        args: &[],
        options: &[Opt {
            long: "--names",
            short: None,
            value: None,
            about: "print only the names, one per line",
        }],
        about: "list saved servers, most recent first",
        details: "",
        examples: &[],
        completes: Complete::Nothing,
        build: |p| Command::List {
            names_only: p.flag("--names"),
        },
    },
    Spec {
        name: "rename",
        alias: Some("mv"),
        group: Group::Servers,
        args: &["<old>", "<new>"],
        options: &[],
        about: "rename a server",
        details: "The saved password moves along with it.",
        examples: &["lssh mv web www"],
        completes: Complete::Server,
        build: |p| Command::Rename {
            old: p.arg(0),
            new: p.arg(1),
        },
    },
    Spec {
        name: "remove",
        alias: Some("rm"),
        group: Group::Servers,
        args: &["<name>"],
        options: &[],
        about: "remove a server and its saved password",
        details: "",
        examples: &["lssh rm web"],
        completes: Complete::Server,
        build: |p| Command::Remove { name: p.arg(0) },
    },
    Spec {
        name: "passwd",
        alias: None,
        group: Group::Servers,
        args: &["<name>"],
        options: &[],
        about: "change a server's saved password",
        details: "Asks for the new password and replaces the one in the OS keyring.\n\
                  lssh also asks by itself when the server rejects the saved one.",
        examples: &["lssh passwd web"],
        completes: Complete::Server,
        build: |p| Command::Passwd { name: p.arg(0) },
    },
    Spec {
        name: "copy-id",
        alias: None,
        group: Group::Servers,
        args: &["<name>"],
        options: &[
            Opt {
                long: "--key",
                short: Some('k'),
                value: Some("<file>"),
                about: "key to install (default: the server's key, or your newest one)",
            },
            Opt {
                long: "--keep-password",
                short: None,
                value: None,
                about: "keep the saved password after switching to the key",
            },
        ],
        about: "switch a server from password to key login",
        details: "Installs your public key on the server with ssh-copy-id, checks that\n\
                  logging in with the key works, then deletes the saved password.",
        examples: &["lssh copy-id web", "lssh copy-id web --key ~/.ssh/id_web"],
        completes: Complete::Server,
        build: |p| Command::CopyId {
            name: p.arg(0),
            key: p.value("--key"),
            keep_password: p.flag("--keep-password"),
        },
    },
    Spec {
        name: "connect",
        alias: Some("c"),
        group: Group::Remote,
        args: &["<name>"],
        options: &[Opt {
            long: "--record",
            short: Some('r'),
            value: None,
            about: "record the session to ~/.local/share/lssh/recordings",
        }],
        about: "connect to a server (or just: lssh <name>)",
        details: "Opens an SSH session with a live stats bar at the bottom of the terminal.\n\
                  Use '-' as the name to reconnect to the last server, or run plain 'lssh'\n\
                  to pick one from a list. Recordings play back with 'asciinema play'.",
        examples: &["lssh web", "lssh -", "lssh connect web --record"],
        completes: Complete::Server,
        build: |p| Command::Connect {
            name: p.arg(0),
            record: p.flag("--record"),
        },
    },
    Spec {
        name: "run",
        alias: None,
        group: Group::Remote,
        args: &["<servers>", "<command>..."],
        options: &[],
        about: "run a command on one or more servers",
        details: "<servers> is a name, a comma-separated list or 'all'. With several servers\n\
                  the command runs on all of them at once and each output line is prefixed\n\
                  with the server's name.",
        examples: &[
            "lssh run web uptime",
            "lssh run web,db -- df -h /",
            "lssh run all 'free -m'",
        ],
        completes: Complete::Servers,
        build: |p| Command::Run {
            servers: p.arg(0),
            command: p.rest(1),
        },
    },
    Spec {
        name: "cp",
        alias: None,
        group: Group::Remote,
        args: &["<from>", "<to>"],
        options: &[Opt {
            long: "--recursive",
            short: Some('r'),
            value: None,
            about: "copy directories",
        }],
        about: "copy files to or from a server",
        details: "One side is <server>:<path>, the other a local path.",
        examples: &[
            "lssh cp web:/var/log/syslog .",
            "lssh cp -r ./site web:/srv/www",
        ],
        completes: Complete::RemotePath,
        build: |p| Command::Cp {
            from: p.arg(0),
            to: p.arg(1),
            recursive: p.flag("--recursive"),
        },
    },
    Spec {
        name: "status",
        alias: None,
        group: Group::Remote,
        args: &["[servers]"],
        options: &[],
        about: "check which servers are reachable",
        details: "Opens a connection to each server's SSH port (all servers by default)\n\
                  and shows the latency and SSH version. Exits with status 1 if any\n\
                  server is down.",
        examples: &["lssh status", "lssh status web,db"],
        completes: Complete::Servers,
        build: |p| Command::Status {
            servers: p.optional(0),
        },
    },
    Spec {
        name: "top",
        alias: None,
        group: Group::Remote,
        args: &["[servers]"],
        options: &[],
        about: "live stats of all servers on one screen",
        details: "Shows the stats bar of every server (all by default) together. Servers\n\
                  need a saved password or a key; connect once with 'lssh <name>' to save\n\
                  one. Press q to quit.",
        examples: &["lssh top", "lssh top web,db"],
        completes: Complete::Servers,
        build: |p| Command::Top {
            servers: p.optional(0),
        },
    },
    Spec {
        name: "config",
        alias: None,
        group: Group::Setup,
        args: &["[edit|path]"],
        options: &[],
        about: "show, edit or locate the settings file",
        details: "Without an argument, prints ~/.config/lssh/config.toml (creating it with\n\
                  the defaults first). 'edit' opens it in $VISUAL or $EDITOR and checks it\n\
                  afterwards; 'path' prints only its location.",
        examples: &["lssh config", "lssh config edit"],
        completes: Complete::Choices(&["edit", "path"]),
        build: |p| Command::Config {
            action: p.optional(0),
        },
    },
    Spec {
        name: "completions",
        alias: None,
        group: Group::Setup,
        args: &["<shell>"],
        options: &[],
        about: "print shell completions (bash, zsh or fish)",
        details: "Completes commands, options and saved server names. Install with:\n\
                  \x20 bash  lssh completions bash > ~/.local/share/bash-completion/completions/lssh\n\
                  \x20 zsh   lssh completions zsh > ~/.zfunc/_lssh   (with ~/.zfunc in $fpath)\n\
                  \x20 fish  lssh completions fish > ~/.config/fish/completions/lssh.fish",
        examples: &["lssh completions bash"],
        completes: Complete::Choices(&["bash", "zsh", "fish"]),
        build: |p| Command::Completions { shell: p.arg(0) },
    },
    Spec {
        name: "help",
        alias: None,
        group: Group::Setup,
        args: &["[command]"],
        options: &[],
        about: "show help, or details about one command",
        details: "",
        examples: &["lssh help run"],
        completes: Complete::Command,
        build: |p| Command::Help {
            topic: p.optional(0),
        },
    },
    Spec {
        name: "version",
        alias: None,
        group: Group::Setup,
        args: &[],
        options: &[],
        about: "print the version",
        details: "",
        examples: &[],
        completes: Complete::Nothing,
        build: |_| Command::Version,
    },
];

/// Look up a command by name or alias.
pub fn find(word: &str) -> Option<&'static Spec> {
    COMMANDS
        .iter()
        .find(|spec| spec.name == word || spec.alias == Some(word))
}

/// Names a server can't have, because they mean something else.
pub fn is_reserved(name: &str) -> bool {
    find(name).is_some() || name == "all"
}

/// Arguments and options of one command, after parsing.
#[derive(Debug, Default)]
pub struct Parsed {
    args: Vec<String>,
    options: Vec<(&'static str, Option<String>)>,
}

impl Parsed {
    fn arg(&self, i: usize) -> String {
        self.args[i].clone()
    }

    fn optional(&self, i: usize) -> Option<String> {
        self.args.get(i).cloned()
    }

    fn rest(&self, from: usize) -> Vec<String> {
        self.args[from..].to_vec()
    }

    fn flag(&self, long: &str) -> bool {
        self.options.iter().any(|(name, _)| *name == long)
    }

    fn value(&self, long: &str) -> Option<String> {
        self.options
            .iter()
            .rev()
            .find(|(name, _)| *name == long)
            .and_then(|(_, value)| value.clone())
    }
}

impl Spec {
    /// `lssh <name> <args...>`
    fn usage(&self) -> String {
        let mut usage = format!("lssh {}", self.name);
        if !self.options.is_empty() {
            usage.push_str(" [options]");
        }
        for arg in self.args {
            usage = format!("{usage} {arg}");
        }
        usage
    }

    /// `name[, alias] <args...>`, as shown in the command list.
    fn signature(&self) -> String {
        let mut sig = self.name.to_string();
        if let Some(alias) = self.alias {
            sig = format!("{sig}, {alias}");
        }
        for arg in self.args {
            sig = format!("{sig} {arg}");
        }
        sig
    }

    fn is_variadic(&self) -> bool {
        self.args.last().is_some_and(|a| a.ends_with("..."))
    }

    fn parse(&'static self, words: &[String]) -> Result<Command, UsageError> {
        let error = |message: String| UsageError::new(Some(self.name), message);
        let mut parsed = Parsed::default();
        let mut words = words.iter();

        while let Some(word) = words.next() {
            let at_variadic = self.is_variadic() && parsed.args.len() + 1 == self.args.len();
            if word == "--" || at_variadic {
                // Everything from here on is an argument, dashes and all.
                if word != "--" {
                    parsed.args.push(word.clone());
                }
                parsed.args.extend(words.cloned());
                break;
            }
            if is_help_flag(word) {
                return Ok(Command::Help {
                    topic: Some(self.name.to_string()),
                });
            }
            if !word.starts_with('-') || word == "-" {
                parsed.args.push(word.clone());
                continue;
            }

            let (flag, inline) = match word.split_once('=') {
                Some((flag, value)) if word.starts_with("--") => (flag, Some(value.to_string())),
                _ => (word.as_str(), None),
            };
            let opt = self
                .options
                .iter()
                .find(|opt| opt.matches(flag))
                .ok_or_else(|| error(format!("unknown option '{flag}'")))?;
            let value =
                match (opt.value, inline) {
                    (Some(_), Some(value)) => Some(value),
                    (Some(placeholder), None) => Some(words.next().cloned().ok_or_else(|| {
                        error(format!("{} needs a value: {placeholder}", opt.long))
                    })?),
                    (None, Some(_)) => return Err(error(format!("{} takes no value", opt.long))),
                    (None, None) => None,
                };
            parsed.options.push((opt.long, value));
        }

        self.check_args(&parsed.args)?;
        Ok((self.build)(&parsed))
    }

    fn check_args(&self, args: &[String]) -> Result<(), UsageError> {
        let required = self.args.iter().filter(|a| !a.starts_with('[')).count();
        if args.len() < required {
            return Err(UsageError::new(
                Some(self.name),
                format!("missing {}", self.args[args.len()]),
            ));
        }
        if !self.is_variadic()
            && let Some(extra) = args.get(self.args.len())
        {
            return Err(UsageError::new(
                Some(self.name),
                format!("unexpected argument '{extra}'"),
            ));
        }
        Ok(())
    }
}

/// A command line that could not be parsed.
#[derive(Debug, PartialEq)]
pub struct UsageError {
    message: String,
    /// The command whose usage to show, if one was recognised.
    command: Option<&'static str>,
}

impl UsageError {
    fn new(command: Option<&'static str>, message: String) -> Self {
        UsageError { message, command }
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.message)?;
        match self.command.and_then(find) {
            Some(spec) => write!(f, "usage: {}", spec.usage()),
            None => write!(f, "run 'lssh help' for usage"),
        }
    }
}

fn is_help_flag(arg: &str) -> bool {
    arg == "-h" || arg == "--help"
}

pub fn parse(args: &[String]) -> Result<Command, UsageError> {
    let Some((first, rest)) = args.split_first() else {
        return Ok(Command::Pick);
    };
    match first.as_str() {
        flag if is_help_flag(flag) => return Ok(Command::Help { topic: None }),
        "-V" | "--version" => return Ok(Command::Version),
        _ => {}
    }

    match find(first) {
        Some(spec) => spec.parse(rest),
        // `lssh web` is shorthand for `lssh connect web`, `lssh -` for the
        // last server.
        None if first == "-" || !first.starts_with('-') => find("connect")
            .expect("connect is in the command table")
            .parse(args),
        None => Err(UsageError::new(None, format!("unknown option '{first}'"))),
    }
}

/// Print the overview, or the details of one command.
pub fn help(topic: Option<&str>) -> Result<()> {
    match topic {
        None => print_overview(),
        Some(topic) => match find(topic) {
            Some(spec) => print_command(spec),
            None => bail!("no command named '{topic}'\nrun 'lssh help' to list commands"),
        },
    }
    Ok(())
}

pub fn print_version() {
    println!("lssh {}", env!("CARGO_PKG_VERSION"));
}

const USAGE: [(&str, &str); 3] = [
    ("lssh", "pick a server and connect"),
    ("lssh <name>", "connect to a server"),
    ("lssh <command> [args]", "run a command"),
];

fn print_overview() {
    let width = COMMANDS
        .iter()
        .map(|spec| spec.signature())
        .chain(USAGE.iter().map(|(usage, _)| usage.to_string()))
        .map(|sig| sig.chars().count())
        .max()
        .unwrap_or(0);

    println!(
        "{} {}",
        "lssh".cyan().bold(),
        format!("{} · simple SSH server manager", env!("CARGO_PKG_VERSION")).dimmed()
    );
    println!();
    println!("{}", "Usage".bold());
    for (usage, about) in USAGE {
        print_row(usage, about, width);
    }
    for group in Group::ALL {
        println!();
        println!("{}", group.title().bold());
        for spec in COMMANDS.iter().filter(|spec| spec.group == group) {
            print_row(&spec.signature(), spec.about, width);
        }
    }
    println!();
    println!(
        "{}",
        "Run 'lssh help <command>' for details on a command.".dimmed()
    );
}

fn print_command(spec: &Spec) {
    println!(
        "{} {}",
        format!("lssh {}", spec.name).cyan().bold(),
        format!("· {}", spec.about).dimmed()
    );
    println!();
    println!("{}", "Usage".bold());
    println!("  {}", highlight(&spec.usage()));
    if let Some(alias) = spec.alias {
        let short = spec.usage().replacen(spec.name, alias, 1);
        println!("  {}", highlight(&short));
    }
    if !spec.details.is_empty() {
        println!();
        for line in spec.details.lines() {
            println!("  {line}");
        }
    }
    if !spec.options.is_empty() {
        println!();
        println!("{}", "Options".bold());
        let width = spec
            .options
            .iter()
            .map(|opt| opt.signature().chars().count())
            .max()
            .unwrap_or(0);
        for opt in spec.options {
            print_row(&opt.signature(), opt.about, width);
        }
    }
    if !spec.examples.is_empty() {
        println!();
        println!("{}", "Examples".bold());
        for example in spec.examples {
            println!("  {}", highlight(example));
        }
    }
}

fn print_row(left: &str, about: &str, width: usize) {
    let pad = " ".repeat(width.saturating_sub(left.chars().count()));
    println!("  {}{pad}   {}", highlight(left), about.dimmed());
}

/// Colour a command line: `<placeholders>` yellow, literal words green.
fn highlight(line: &str) -> String {
    line.split(' ')
        .map(|word| {
            if word.starts_with(['<', '[']) {
                word.yellow().to_string()
            } else {
                word.green().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(line: &str) -> Result<Command, UsageError> {
        let args: Vec<String> = line.split_whitespace().map(String::from).collect();
        parse(&args)
    }

    fn error_of(line: &str) -> String {
        parse_str(line).unwrap_err().message
    }

    fn connect(name: &str) -> Command {
        Command::Connect {
            name: name.into(),
            record: false,
        }
    }

    #[test]
    fn parses_add_with_and_without_key() {
        assert_eq!(
            parse_str("add web root@1.2.3.4"),
            Ok(Command::Add {
                name: "web".into(),
                target: "root@1.2.3.4".into(),
                key: None,
            })
        );
        for line in [
            "add web root@1.2.3.4 --key ~/k",
            "add --key ~/k web root@1.2.3.4",
            "add web -k ~/k root@1.2.3.4",
            "add web root@1.2.3.4 --key=~/k",
        ] {
            assert_eq!(
                parse_str(line),
                Ok(Command::Add {
                    name: "web".into(),
                    target: "root@1.2.3.4".into(),
                    key: Some("~/k".into()),
                }),
                "{line}"
            );
        }
    }

    #[test]
    fn parses_names_and_aliases() {
        assert_eq!(parse_str("ls"), Ok(Command::List { names_only: false }));
        assert_eq!(
            parse_str("list --names"),
            Ok(Command::List { names_only: true })
        );
        for line in ["c web", "connect web", "web"] {
            assert_eq!(parse_str(line), Ok(connect("web")));
        }
        for line in ["rm web", "remove web"] {
            assert_eq!(parse_str(line), Ok(Command::Remove { name: "web".into() }));
        }
        assert_eq!(
            parse_str("mv a b"),
            Ok(Command::Rename {
                old: "a".into(),
                new: "b".into()
            })
        );
        assert_eq!(
            parse_str("config edit"),
            Ok(Command::Config {
                action: Some("edit".into())
            })
        );
    }

    #[test]
    fn connect_shorthand_last_server_and_record() {
        assert_eq!(parse_str("-"), Ok(connect("-")));
        assert_eq!(parse_str("c -"), Ok(connect("-")));
        let recording = Ok(Command::Connect {
            name: "web".into(),
            record: true,
        });
        assert_eq!(parse_str("web --record"), recording);
        assert_eq!(parse_str("connect -r web"), recording);
    }

    #[test]
    fn plain_lssh_picks() {
        assert_eq!(parse_str(""), Ok(Command::Pick));
    }

    #[test]
    fn run_takes_the_rest_verbatim() {
        let run = |servers: &str, command: &[&str]| {
            Ok(Command::Run {
                servers: servers.into(),
                command: command.iter().map(|s| s.to_string()).collect(),
            })
        };
        assert_eq!(parse_str("run web uptime"), run("web", &["uptime"]));
        assert_eq!(parse_str("run web df -h /"), run("web", &["df", "-h", "/"]));
        assert_eq!(
            parse_str("run web,db -- ls -la"),
            run("web,db", &["ls", "-la"])
        );
        assert_eq!(parse_str("run all --help"), run("all", &["--help"]));
        assert_eq!(error_of("run web"), "missing <command>...");
        assert_eq!(
            parse_str("run --help"),
            Ok(Command::Help {
                topic: Some("run".into())
            })
        );
    }

    #[test]
    fn parses_other_commands() {
        assert_eq!(
            parse_str("cp -r web:/etc ."),
            Ok(Command::Cp {
                from: "web:/etc".into(),
                to: ".".into(),
                recursive: true
            })
        );
        assert_eq!(
            parse_str("copy-id web --keep-password"),
            Ok(Command::CopyId {
                name: "web".into(),
                key: None,
                keep_password: true
            })
        );
        assert_eq!(parse_str("status"), Ok(Command::Status { servers: None }));
        assert_eq!(
            parse_str("top web,db"),
            Ok(Command::Top {
                servers: Some("web,db".into())
            })
        );
        assert_eq!(parse_str("import"), Ok(Command::Import { file: None }));
        assert_eq!(
            parse_str("completions zsh"),
            Ok(Command::Completions {
                shell: "zsh".into()
            })
        );
    }

    #[test]
    fn parses_help_and_version() {
        for line in ["help", "-h", "--help"] {
            assert_eq!(parse_str(line), Ok(Command::Help { topic: None }));
        }
        for line in ["help add", "add --help", "add web -h"] {
            assert_eq!(
                parse_str(line),
                Ok(Command::Help {
                    topic: Some("add".into())
                })
            );
        }
        for line in ["version", "-V", "--version"] {
            assert_eq!(parse_str(line), Ok(Command::Version));
        }
    }

    #[test]
    fn missing_arguments_are_named() {
        assert_eq!(error_of("add"), "missing <name>");
        assert_eq!(error_of("add web"), "missing <user@host[:port]>");
        assert_eq!(error_of("c"), "missing <name>");
        assert_eq!(error_of("rm"), "missing <name>");
        assert_eq!(
            error_of("add web root@h --key"),
            "--key needs a value: <file>"
        );
    }

    #[test]
    fn extra_arguments_and_options_are_rejected() {
        assert_eq!(error_of("ls web"), "unexpected argument 'web'");
        assert_eq!(error_of("help add rm"), "unexpected argument 'rm'");
        assert_eq!(error_of("web uptime"), "unexpected argument 'uptime'");
        assert_eq!(error_of("-x"), "unknown option '-x'");
        assert_eq!(error_of("rm -f web"), "unknown option '-f'");
        assert_eq!(error_of("ls --names=yes"), "--names takes no value");
    }

    #[test]
    fn usage_error_shows_the_command_usage() {
        let err = parse_str("add web").unwrap_err().to_string();
        assert_eq!(
            err,
            "missing <user@host[:port]>\nusage: lssh add [options] <name> <user@host[:port]>"
        );
        let err = parse_str("-x").unwrap_err().to_string();
        assert!(err.ends_with("run 'lssh help' for usage"));
    }

    #[test]
    fn reserved_names() {
        assert!(is_reserved("list"));
        assert!(is_reserved("ls"));
        assert!(is_reserved("all"));
        assert!(!is_reserved("web"));
    }

    #[test]
    fn command_table_is_consistent() {
        let mut words: Vec<&str> = COMMANDS
            .iter()
            .flat_map(|spec| std::iter::once(spec.name).chain(spec.alias))
            .collect();
        let total = words.len();
        words.sort_unstable();
        words.dedup();
        assert_eq!(
            words.len(),
            total,
            "command names and aliases must be unique"
        );

        for spec in COMMANDS {
            let first_optional = spec.args.iter().position(|a| a.starts_with('['));
            if let Some(i) = first_optional {
                assert!(
                    spec.args[i..].iter().all(|a| a.starts_with('[')),
                    "optional arguments of '{}' must come last",
                    spec.name
                );
            }
            if let Some(i) = spec.args.iter().position(|a| a.ends_with("...")) {
                assert_eq!(
                    i + 1,
                    spec.args.len(),
                    "'{}': variadic must be last",
                    spec.name
                );
            }
            for opt in spec.options {
                assert!(opt.long.starts_with("--"), "{}", opt.long);
            }
        }
    }
}
