use clap::{Parser, Subcommand};

/// wonk - code search and indexing tool
#[derive(Parser, Debug)]
#[command(name = "wonk", version, about)]
pub struct Cli {
    /// Output results as JSON
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Full-text search across indexed repositories
    Search(SearchArgs),

    /// Look up symbols (functions, classes, variables, etc.)
    Sym(SymArgs),

    /// Find references to a symbol
    Ref(RefArgs),

    /// Show function/method signatures
    Sig(SigArgs),

    /// List files in the index
    Ls(LsArgs),

    /// Show dependencies of a file
    Deps(DepsArgs),

    /// Show reverse dependencies (files that depend on a given file)
    Rdeps(RdepsArgs),

    /// Initialize indexing for the current repository
    Init(InitArgs),

    /// Update the index for the current repository
    Update,

    /// Show indexing status for the current repository
    Status,

    /// Manage the background daemon
    Daemon(DaemonArgs),

    /// Manage tracked repositories
    Repos(ReposArgs),
}

#[derive(clap::Args, Debug)]
pub struct SearchArgs {
    /// The search pattern
    pub pattern: String,

    /// Treat the pattern as a regular expression
    #[arg(long)]
    pub regex: bool,

    /// Case-insensitive search
    #[arg(short = 'i', long)]
    pub ignore_case: bool,

    /// Restrict search to these paths (use -- before paths)
    #[arg(last = true)]
    pub paths: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct SymArgs {
    /// Symbol name to look up
    pub name: String,

    /// Filter by symbol kind (e.g. function, class, variable)
    #[arg(long)]
    pub kind: Option<String>,

    /// Require an exact match on the symbol name
    #[arg(long)]
    pub exact: bool,
}

#[derive(clap::Args, Debug)]
pub struct RefArgs {
    /// Symbol name to find references for
    pub name: String,

    /// Restrict search to these paths (use -- before paths)
    #[arg(last = true)]
    pub paths: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct SigArgs {
    /// Function or method name
    pub name: String,
}

#[derive(clap::Args, Debug)]
pub struct LsArgs {
    /// Path to list (defaults to repository root)
    #[arg(default_value = ".")]
    pub path: String,

    /// Show files in a tree structure
    #[arg(long)]
    pub tree: bool,
}

#[derive(clap::Args, Debug)]
pub struct DepsArgs {
    /// File to show dependencies for
    pub file: String,
}

#[derive(clap::Args, Debug)]
pub struct RdepsArgs {
    /// File to show reverse dependencies for
    pub file: String,
}

#[derive(clap::Args, Debug)]
pub struct InitArgs {
    /// Use a local (project-specific) index instead of the shared index
    #[arg(long)]
    pub local: bool,
}

#[derive(clap::Args, Debug)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Start the background daemon
    Start,
    /// Stop the background daemon
    Stop,
    /// Show the daemon status
    Status,
}

#[derive(clap::Args, Debug)]
pub struct ReposArgs {
    #[command(subcommand)]
    pub command: ReposCommand,
}

#[derive(Subcommand, Debug)]
pub enum ReposCommand {
    /// List all tracked repositories
    List,
    /// Remove stale repositories from the index
    Clean,
}

pub fn parse() -> Cli {
    Cli::parse()
}
