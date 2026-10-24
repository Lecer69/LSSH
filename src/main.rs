mod askpass;
mod cli;
mod commands;
mod completions;
mod config;
mod import;
mod out;
mod password;
mod picker;
mod proxy;
mod record;
mod remote;
mod session;
mod stats;
mod store;
mod top;
mod tui;
mod ui;
mod vt;

use std::process::ExitCode;

use anyhow::Result;

use cli::Command;

fn main() -> ExitCode {
    // Die quietly on a closed pipe (`lssh ls | head`) like other CLI tools,
    // instead of panicking in println!.
    // SAFETY: called first thing, before any other threads exist.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // ssh runs lssh as its ProxyCommand to go through a proxy. That process
    // inherits the askpass variables too, so this is checked first.
    if proxy::invoked() {
        return proxy::main();
    }
    // ssh runs lssh as its SSH_ASKPASS helper to ask for passwords.
    if askpass::invoked() {
        return askpass::main();
    }

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
        Err(e) => match e.downcast_ref::<out::Exit>() {
            Some(out::Exit(code)) => ExitCode::from(*code),
            None => {
                out::error(&format!("{e:#}"));
                ExitCode::FAILURE
            }
        },
    }
}

fn run(command: Command) -> Result<()> {
    match command {
        Command::Pick { proxy } => commands::pick(proxy),
        Command::Add { name, target, key } => commands::add(&name, &target, key.as_deref()),
        Command::Import { file } => commands::import(file.as_deref()),
        Command::List { names_only } => commands::list(names_only),
        Command::Rename { old, new } => commands::rename(&old, &new),
        Command::Remove { name } => commands::remove(&name),
        Command::Passwd { name } => commands::passwd(&name),
        Command::CopyId {
            name,
            key,
            keep_password,
            proxy,
        } => remote::copy_id(&name, key.as_deref(), keep_password, proxy),
        Command::Connect {
            name,
            record,
            proxy,
        } => commands::connect(&name, record, proxy),
        Command::Run {
            servers,
            command,
            proxy,
        } => remote::run(&servers, &command, proxy),
        Command::Cp {
            from,
            to,
            recursive,
            proxy,
        } => remote::cp(&from, &to, recursive, proxy),
        Command::Status { servers, proxy } => remote::status(servers.as_deref(), proxy),
        Command::Top { servers, proxy } => commands::top(servers.as_deref(), proxy),
        Command::Config { action } => commands::config(action.as_deref()),
        Command::Proxy { action, address } => {
            commands::proxy(action.as_deref(), address.as_deref())
        }
        Command::Completions { shell } => commands::completions(&shell),
        Command::Help { topic } => cli::help(topic.as_deref()),
        Command::Version => {
            cli::print_version();
            Ok(())
        }
    }
}
