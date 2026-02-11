mod cli;
mod config;
mod daemon;
mod db;
mod errors;
mod indexer;
mod router;
mod search;
mod types;

use anyhow::Result;

fn main() -> Result<()> {
    let cli = cli::parse();
    router::dispatch(cli)
}
