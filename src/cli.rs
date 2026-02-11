use clap::Parser;

/// wonk - code search and indexing tool
#[derive(Parser, Debug)]
#[command(name = "wonk", version, about)]
pub struct Args {
    /// Placeholder subcommand
    #[arg(default_value = "help")]
    pub command: String,
}

pub fn parse() -> Args {
    Args::parse()
}
