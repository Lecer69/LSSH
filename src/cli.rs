use colored::Colorize;

pub fn print_help() {
    println!("{}", "LSSH".bold().cyan());
    println!("{}", "Simple SSH server manager".dimmed());
    println!();
    println!(
        "  {} {}  {}",
        "add".green().bold(),
        "<name> <user@host>".yellow(),
        "add a server".dimmed()
    );
    println!(
        "     {} e.g. {} or {}",
        "format:".dimmed(),
        "root@11.12.13.14".yellow(),
        "11.12.13.14@root".yellow()
    );
    println!(
        "  {}            {}",
        "ls".green().bold(),
        "list all saved servers".dimmed()
    );
    println!(
        "  {}   {}  {}",
        "c, connect".green().bold(),
        "<name>".yellow(),
        "connect to a server".dimmed()
    );
    println!(
        "  {}          {}",
        "<name>".green().bold(),
        "shorthand for 'lssh c <name>'".dimmed()
    );
    println!(
        "  {} {}  {}",
        "rm, remove".green().bold(),
        "<name>".yellow(),
        "remove a server".dimmed()
    );
    println!(
        "  {}          {}",
        "help".green().bold(),
        "show this help".dimmed()
    );
    println!();
}

/// Parsed subcommand.
#[derive(Debug, PartialEq)]
pub enum Command {
    Add { name: String, target: String },
    List,
    Connect { name: String },
    Remove { name: String },
    Help,
}

pub fn parse_args(args: &[String]) -> Command {
    let Some(cmd) = args.first().map(String::as_str) else {
        return Command::Help;
    };

    match cmd {
        "add" => match (args.get(1), args.get(2)) {
            (Some(name), Some(target)) => Command::Add {
                name: name.clone(),
                target: target.clone(),
            },
            _ => Command::Help,
        },
        "ls" | "list" => Command::List,
        "c" | "connect" => match args.get(1) {
            Some(name) => Command::Connect { name: name.clone() },
            None => Command::Help,
        },
        "rm" | "remove" => match args.get(1) {
            Some(name) => Command::Remove { name: name.clone() },
            None => Command::Help,
        },
        "help" | "--help" | "-h" => Command::Help,
        // `lssh web` is shorthand for `lssh c web`.
        _ if args.len() == 1 => Command::Connect {
            name: cmd.to_string(),
        },
        _ => Command::Help,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_add() {
        let cmd = parse_args(&args(&["add", "web", "root@1.2.3.4"]));
        assert_eq!(
            cmd,
            Command::Add {
                name: "web".into(),
                target: "root@1.2.3.4".into()
            }
        );
    }

    #[test]
    fn parses_aliases() {
        assert_eq!(parse_args(&args(&["ls"])), Command::List);
        assert_eq!(parse_args(&args(&["list"])), Command::List);
        assert_eq!(
            parse_args(&args(&["c", "web"])),
            Command::Connect { name: "web".into() }
        );
        assert_eq!(
            parse_args(&args(&["rm", "web"])),
            Command::Remove { name: "web".into() }
        );
    }

    #[test]
    fn missing_args_fall_back_to_help() {
        assert_eq!(parse_args(&args(&[])), Command::Help);
        assert_eq!(parse_args(&args(&["add", "web"])), Command::Help);
        assert_eq!(parse_args(&args(&["c"])), Command::Help);
        assert_eq!(parse_args(&args(&["rm"])), Command::Help);
    }

    #[test]
    fn bare_server_name_is_shorthand_for_connect() {
        assert_eq!(
            parse_args(&args(&["web"])),
            Command::Connect { name: "web".into() }
        );
    }

    #[test]
    fn unknown_multi_arg_command_is_help() {
        assert_eq!(parse_args(&args(&["frobnicate", "extra"])), Command::Help);
    }
}
