mod cli;
mod config;
mod daemon;
mod db;
mod errors;
mod indexer;
mod output;
mod pipeline;
mod router;
mod search;
mod types;
mod walker;
mod watcher;

use anyhow::Result;

fn main() -> Result<()> {
    let cli = cli::parse();
    router::dispatch(cli)
}
