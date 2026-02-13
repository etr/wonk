mod cli;
mod color;
mod config;
mod daemon;
mod db;
mod errors;
mod indexer;
mod output;
mod pipeline;
mod progress;
mod ranker;
mod router;
mod search;
mod types;
mod walker;
mod watcher;

use std::process;

fn main() {
    // Parse CLI first so we know whether --json was requested.
    // clap handles its own usage errors (exit code 2) before we get here.
    let cli = cli::parse();
    let json = cli.json;

    match router::dispatch(cli) {
        Ok(()) => process::exit(errors::EXIT_SUCCESS),
        Err(err) => {
            let wonk_err: errors::WonkError = err.into();
            let code = output::format_error(&wonk_err, json);
            process::exit(code);
        }
    }
}
