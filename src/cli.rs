//! Command-line interface: the command table, parsing and help screens.
//!
//! Every command is described once in [`COMMANDS`]; the parser and both help
//! screens are generated from it, so they cannot drift apart.

use std::fmt;

use anyhow::{Result, bail};
use colored::Colorize;

/// A parsed command line.
#[derive(Debug, PartialEq)]
pub enum Command {
    Add { name: String, target: String },
    List,
    Connect { name: String },
    Remove { name: String },
    Passwd { name: String },
    Config,
    Help { topic: Option<String> },
    Version,
}

/// One entry of the command table.
pub struct Spec {
    pub name: &'static str,
    pub alias: Option<&'static str>,
    /// Positional arguments. Optional ones are written `[like this]` and
    /// must come last.
    pub args: &'static [&'static str],
    /// One-line summary for the command list.
    pub about: &'static str,
    /// Longer explanation for `lssh help <command>`.
    pub details: &'static str,
    pub examples: &'static [&'static str],
    /// Turn the (already counted) arguments into a [`Command`].
    build: fn(&[String]) -> Command,
}

pub const COMMANDS: &[Spec] = &[
    Spec {
        name: "add",
        alias: None,
        args: &["<name>", "<user@host>"],
        about: "save a new server",
        details: "The target may also be written host@user when the host is an IP address.",
        examples: &[
            "lssh add web root@11.12.13.14",
            "lssh add db 10.0.0.5@admin",
        ],
        build: |a| Command::Add {
            name: a[0].clone(),
            target: a[1].clone(),
        },
    },
    Spec {
        name: "list",
        alias: Some("ls"),
        args: &[],
        about: "list saved servers",
        details: "",
        examples: &[],
        build: |_| Command::List,
    },
    Spec {
        name: "connect",
        alias: Some("c"),
        args: &["<name>"],
        about: "connect to a saved server",
        details: "Opens an SSH session with a live stats bar at the bottom of the terminal.\n\
                  The password is asked once and kept in the OS keyring (needs sshpass).\n\
                  Plain 'lssh <name>' does the same.",
        examples: &["lssh web", "lssh connect web"],
        build: |a| Command::Connect { name: a[0].clone() },
    },
    Spec {
        name: "remove",
        alias: Some("rm"),
        args: &["<name>"],
        about: "remove a server and its saved password",
        details: "",
        examples: &["lssh rm web"],
        build: |a| Command::Remove { name: a[0].clone() },
    },
    Spec {
        name: "passwd",
        alias: None,
        args: &["<name>"],
        about: "change a server's saved password",
        details: "Asks for the new password and replaces the one in the OS keyring.\n\
                  Use it after the password was changed on the server.",
        examples: &["lssh passwd web"],
        build: |a| Command::Passwd { name: a[0].clone() },
    },
    Spec {
        name: "config",
        alias: None,
        args: &[],
        about: "show the settings file",
        details: "Prints ~/.config/lssh/config.toml, creating it with the defaults first\n\
                  if needed. Edit that file to change the stats bar.",
        examples: &[],
        build: |_| Command::Config,
    },
    Spec {
        name: "help",
        alias: None,
        args: &["[command]"],
        about: "show help, or details about one command",
        details: "",
        examples: &["lssh help add"],
        build: |a| Command::Help {
            topic: a.first().cloned(),
        },
    },
    Spec {
        name: "version",
        alias: None,
        args: &[],
        about: "print the version",
        details: "",
        examples: &[],
        build: |_| Command::Version,
    },
];

/// Look up a command by name or alias.
pub fn find(word: &str) -> Option<&'static Spec> {
    COMMANDS
        .iter()
        .find(|spec| spec.name == word || spec.alias == Some(word))
}

impl Spec {
    /// `lssh <name> <args...>`
    fn usage(&self) -> String {
        std::iter::once("lssh")
            .chain(std::iter::once(self.name))
            .chain(self.args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ")
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

    fn check_args(&self, args: &[String]) -> Result<(), UsageError> {
        let required = self.args.iter().filter(|a| !a.starts_with('[')).count();
        if args.len() < required {
            return Err(UsageError::new(
                Some(self.name),
                format!("missing {}", self.args[args.len()]),
            ));
        }
        if let Some(extra) = args.get(self.args.len()) {
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
        return Ok(Command::Help { topic: None });
    };
    match first.as_str() {
        flag if is_help_flag(flag) => return Ok(Command::Help { topic: None }),
        "-V" | "--version" => return Ok(Command::Version),
        flag if flag.starts_with('-') => {
            return Err(UsageError::new(None, format!("unknown option '{flag}'")));
        }
        _ => {}
    }

    let Some(spec) = find(first) else {
        // `lssh web` is shorthand for `lssh connect web`.
        return match rest {
            [] => Ok(Command::Connect {
                name: first.clone(),
            }),
            [extra, ..] => Err(UsageError::new(
                None,
                format!("unexpected argument '{extra}' after server name '{first}'"),
            )),
        };
    };

    if rest.iter().any(|a| is_help_flag(a)) {
        return Ok(Command::Help {
            topic: Some(spec.name.to_string()),
        });
    }
    if let Some(flag) = rest.iter().find(|a| a.starts_with('-')) {
        return Err(UsageError::new(
            Some(spec.name),
            format!("unknown option '{flag}'"),
        ));
    }
    spec.check_args(rest)?;
    Ok((spec.build)(rest))
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

const SHORTHAND: (&str, &str) = ("lssh <name>", "connect to a saved server");
const GENERIC: (&str, &str) = ("lssh <command> [args]", "run a command");

fn print_overview() {
    let rows: Vec<(String, &str)> = COMMANDS
        .iter()
        .map(|spec| (spec.signature(), spec.about))
        .collect();
    let width = rows
        .iter()
        .map(|(sig, _)| sig.as_str())
        .chain([SHORTHAND.0, GENERIC.0])
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
    for (usage, about) in [SHORTHAND, GENERIC] {
        print_row(usage, about, width);
    }
    println!();
    println!("{}", "Commands".bold());
    for (sig, about) in &rows {
        print_row(sig, about, width);
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

    #[test]
    fn parses_add() {
        assert_eq!(
            parse_str("add web root@1.2.3.4"),
            Ok(Command::Add {
                name: "web".into(),
                target: "root@1.2.3.4".into()
            })
        );
    }

    #[test]
    fn parses_names_and_aliases() {
        assert_eq!(parse_str("ls"), Ok(Command::List));
        assert_eq!(parse_str("list"), Ok(Command::List));
        for line in ["c web", "connect web", "web"] {
            assert_eq!(parse_str(line), Ok(Command::Connect { name: "web".into() }));
        }
        for line in ["rm web", "remove web"] {
            assert_eq!(parse_str(line), Ok(Command::Remove { name: "web".into() }));
        }
        assert_eq!(
            parse_str("passwd web"),
            Ok(Command::Passwd { name: "web".into() })
        );
        assert_eq!(parse_str("config"), Ok(Command::Config));
    }

    #[test]
    fn parses_help_and_version() {
        for line in ["", "help", "-h", "--help"] {
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
        assert_eq!(error_of("add web"), "missing <user@host>");
        assert_eq!(error_of("c"), "missing <name>");
        assert_eq!(error_of("rm"), "missing <name>");
    }

    #[test]
    fn extra_arguments_and_options_are_rejected() {
        assert_eq!(error_of("ls web"), "unexpected argument 'web'");
        assert_eq!(error_of("help add rm"), "unexpected argument 'rm'");
        assert!(error_of("web uptime").contains("unexpected argument 'uptime'"));
        assert_eq!(error_of("-x"), "unknown option '-x'");
        assert_eq!(error_of("rm -f web"), "unknown option '-f'");
    }

    #[test]
    fn usage_error_shows_the_command_usage() {
        let err = parse_str("add web").unwrap_err().to_string();
        assert_eq!(
            err,
            "missing <user@host>\nusage: lssh add <name> <user@host>"
        );
        let err = parse_str("-x").unwrap_err().to_string();
        assert!(err.ends_with("run 'lssh help' for usage"));
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
        }
    }
}
