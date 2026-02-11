use anyhow::Result;

use crate::cli::{Cli, Command, DaemonCommand, ReposCommand};
use crate::search;

pub fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Search(args) => {
            let results = search::text_search(
                &args.pattern,
                args.regex,
                args.ignore_case,
                &args.paths,
            )?;

            if cli.json {
                // JSON output: one JSON object per line.
                for r in &results {
                    let obj = serde_json::json!({
                        "file": r.file.to_string_lossy(),
                        "line": r.line,
                        "col": r.col,
                        "content": r.content,
                    });
                    println!("{}", obj);
                }
            } else {
                // Human-readable output matching ripgrep style:
                // file:line:content
                for r in &results {
                    println!(
                        "{}:{}:{}",
                        r.file.display(),
                        r.line,
                        r.content,
                    );
                }
            }
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
