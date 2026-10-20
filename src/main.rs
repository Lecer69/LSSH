mod cli;
mod commands;
mod config;
mod hostkey;
mod out;
mod password;
mod session;
mod stats;
mod store;
mod ui;
mod vt;

use std::process::ExitCode;

use anyhow::Result;

use cli::Command;

fn main() -> ExitCode {
    out::init();
    let args: Vec<String> = std::env::args().skip(1).collect();

    let command = match cli::parse(&args) {
        Ok(command) => command,
        Err(usage) => {
            out::error(&usage.to_string());
            return ExitCode::from(2);
        }
    };
    match run(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            out::error(&format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<()> {
    match command {
        Command::Add { name, target } => commands::add(&name, &target),
        Command::List => commands::list(),
        Command::Connect { name } => commands::connect(&name),
        Command::Remove { name } => commands::remove(&name),
        Command::Passwd { name } => commands::passwd(&name),
        Command::Config => commands::show_config(),
        Command::Help { topic } => cli::help(topic.as_deref()),
        Command::Version => {
            cli::print_version();
            Ok(())
        }
    }
}
