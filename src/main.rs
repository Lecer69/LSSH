mod cli;
mod commands;
mod store;

use anyhow::Result;

use cli::Command;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = store::config_path()?;

    match cli::parse_args(&args) {
        Command::Add { name, target } => commands::add(&path, &name, &target),
        Command::List => commands::list(&path),
        Command::Connect { name } => commands::connect(&path, &name),
        Command::Remove { name } => commands::remove(&path, &name),
        Command::Help => {
            cli::print_help();
            Ok(())
        }
    }
}
