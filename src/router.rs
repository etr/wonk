use std::io;

use anyhow::Result;

use crate::cli::{Cli, Command, DaemonCommand, ReposCommand};
use crate::output::{self, Formatter, SearchOutput};
use crate::search;

pub fn dispatch(cli: Cli) -> Result<()> {
    let stdout = io::stdout().lock();
    let mut fmt = Formatter::new(stdout, cli.json);

    match cli.command {
        Command::Search(args) => {
            let results = search::text_search(
                &args.pattern,
                args.regex,
                args.ignore_case,
                &args.paths,
            )?;

            for r in &results {
                let out = SearchOutput::from_search_result(
                    &r.file, r.line, r.col, &r.content,
                );
                fmt.format_search_result(&out)?;
            }
        }
        Command::Sym(_args) => {
            output::print_hint("sym: not yet implemented");
        }
        Command::Ref(_args) => {
            output::print_hint("ref: not yet implemented");
        }
        Command::Sig(_args) => {
            output::print_hint("sig: not yet implemented");
        }
        Command::Ls(_args) => {
            output::print_hint("ls: not yet implemented");
        }
        Command::Deps(_args) => {
            output::print_hint("deps: not yet implemented");
        }
        Command::Rdeps(_args) => {
            output::print_hint("rdeps: not yet implemented");
        }
        Command::Init(_args) => {
            output::print_hint("init: not yet implemented");
        }
        Command::Update => {
            output::print_hint("update: not yet implemented");
        }
        Command::Status => {
            output::print_hint("status: not yet implemented");
        }
        Command::Daemon(args) => match args.command {
            DaemonCommand::Start => {
                output::print_hint("daemon start: not yet implemented");
            }
            DaemonCommand::Stop => {
                output::print_hint("daemon stop: not yet implemented");
            }
            DaemonCommand::Status => {
                output::print_hint("daemon status: not yet implemented");
            }
        },
        Command::Repos(args) => match args.command {
            ReposCommand::List => {
                output::print_hint("repos list: not yet implemented");
            }
            ReposCommand::Clean => {
                output::print_hint("repos clean: not yet implemented");
            }
        },
    }
    Ok(())
}
