mod cli;
mod config;
mod daemon;
mod db;
mod errors;
mod indexer;
mod output;
mod router;
mod search;
mod types;
mod walker;

use anyhow::Result;

fn main() -> Result<()> {
    let cli = cli::parse();
    router::dispatch(cli)
}
