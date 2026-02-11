use anyhow::Result;

use crate::cli::{Cli, Command, DaemonCommand, ReposCommand};

pub fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Search(_args) => {
            eprintln!("search: not yet implemented");
        }
        Command::Sym(_args) => {
            eprintln!("sym: not yet implemented");
        }
        Command::Ref(_args) => {
            eprintln!("ref: not yet implemented");
        }
        Command::Sig(_args) => {
            eprintln!("sig: not yet implemented");
        }
        Command::Ls(_args) => {
            eprintln!("ls: not yet implemented");
        }
        Command::Deps(_args) => {
            eprintln!("deps: not yet implemented");
        }
        Command::Rdeps(_args) => {
            eprintln!("rdeps: not yet implemented");
        }
        Command::Init(_args) => {
            eprintln!("init: not yet implemented");
        }
        Command::Update => {
            eprintln!("update: not yet implemented");
        }
        Command::Status => {
            eprintln!("status: not yet implemented");
        }
        Command::Daemon(args) => match args.command {
            DaemonCommand::Start => {
                eprintln!("daemon start: not yet implemented");
            }
            DaemonCommand::Stop => {
                eprintln!("daemon stop: not yet implemented");
            }
            DaemonCommand::Status => {
                eprintln!("daemon status: not yet implemented");
            }
        },
        Command::Repos(args) => match args.command {
            ReposCommand::List => {
                eprintln!("repos list: not yet implemented");
            }
            ReposCommand::Clean => {
                eprintln!("repos clean: not yet implemented");
            }
        },
    }
    Ok(())
}
