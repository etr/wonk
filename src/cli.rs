use clap::{Parser, Subcommand};

use crate::output::OutputFormat;

/// wonk - code search and indexing tool
#[derive(Parser, Debug)]
#[command(name = "wonk", version, about)]
pub struct Cli {
    /// Output format: grep (default), json, or toon
    #[arg(long, global = true, value_enum)]
    pub format: Option<OutputFormat>,

    /// Suppress hint messages on stderr
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// Limit output to approximately N tokens (higher-ranked results preserved)
    #[arg(long, global = true)]
    pub budget: Option<usize>,

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

    /// Run MCP (Model Context Protocol) server
    Mcp(McpArgs),

    /// Semantic search: find symbols related to a natural language query
    Ask(AskArgs),
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

    /// Output raw results without ranking, deduplication, or category headers
    #[arg(long, conflicts_with = "smart")]
    pub raw: bool,

    /// Force smart ranking mode even if pattern does not match known symbols
    #[arg(long, conflicts_with = "raw")]
    pub smart: bool,

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
    Stop(DaemonStopArgs),
    /// Show the daemon status
    Status,
    /// List all running daemons
    List,
}

#[derive(clap::Args, Debug)]
pub struct DaemonStopArgs {
    /// Stop all running daemons across all repositories
    #[arg(long)]
    pub all: bool,
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

#[derive(clap::Args, Debug)]
pub struct AskArgs {
    /// The semantic search query
    pub query: String,
    /// Restrict results to symbols reachable from this file
    #[arg(long)]
    pub from: Option<String>,
    /// Restrict results to symbols that can reach this file
    #[arg(long)]
    pub to: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: McpCommand,
}

#[derive(Subcommand, Debug)]
pub enum McpCommand {
    /// Start the MCP server (stdio transport)
    Serve,
}

pub fn parse() -> Cli {
    Cli::parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_ask_basic_query() {
        let cli = Cli::try_parse_from(["wonk", "ask", "authentication"]).unwrap();
        match cli.command {
            Command::Ask(args) => {
                assert_eq!(args.query, "authentication");
                assert!(args.from.is_none());
                assert!(args.to.is_none());
            }
            _ => panic!("expected Command::Ask"),
        }
    }

    #[test]
    fn parse_ask_with_from_and_to() {
        let cli = Cli::try_parse_from(["wonk", "ask", "query", "--from", "a.rs", "--to", "b.rs"])
            .unwrap();
        match cli.command {
            Command::Ask(args) => {
                assert_eq!(args.query, "query");
                assert_eq!(args.from.as_deref(), Some("a.rs"));
                assert_eq!(args.to.as_deref(), Some("b.rs"));
            }
            _ => panic!("expected Command::Ask"),
        }
    }

    #[test]
    fn parse_ask_with_global_budget() {
        let cli = Cli::try_parse_from(["wonk", "--budget", "500", "ask", "query"]).unwrap();
        assert_eq!(cli.budget, Some(500));
        match cli.command {
            Command::Ask(args) => {
                assert_eq!(args.query, "query");
            }
            _ => panic!("expected Command::Ask"),
        }
    }

    #[test]
    fn parse_ask_requires_query() {
        let result = Cli::try_parse_from(["wonk", "ask"]);
        assert!(result.is_err());
    }
}
