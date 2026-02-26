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

    /// Cluster symbols by semantic similarity within a directory
    Cluster(ClusterArgs),

    /// Analyze impact of changed symbols via semantic similarity
    Impact(ImpactArgs),

    /// Show full source body of a symbol
    Show(ShowArgs),
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

    /// Blend structural results with semantic (embedding-based) results
    #[arg(long, conflicts_with = "raw")]
    pub semantic: bool,

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
    /// Restrict results to symbols reachable from this file (dependency scoping)
    #[arg(long)]
    pub from: Option<String>,
    /// Restrict results to symbols that can reach this file (reverse dependency scoping)
    #[arg(long)]
    pub to: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct ClusterArgs {
    /// Directory path to cluster symbols from
    pub path: String,
    /// Number of representative symbols to show per cluster (default: 5)
    #[arg(long, default_value_t = 5)]
    pub top: usize,
}

#[derive(clap::Args, Debug)]
pub struct ImpactArgs {
    /// File to analyze for changed symbols
    pub file: String,

    /// Analyze all files changed since this commit (e.g. HEAD~3)
    #[arg(long)]
    pub since: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct ShowArgs {
    /// Symbol name to look up
    pub name: String,

    /// Restrict results to a specific file path
    #[arg(long)]
    pub file: Option<String>,

    /// Filter by symbol kind (e.g. function, class, variable)
    #[arg(long)]
    pub kind: Option<String>,

    /// Require an exact match on the symbol name
    #[arg(long)]
    pub exact: bool,
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

    #[test]
    fn parse_search_semantic_flag() {
        let cli = Cli::try_parse_from(["wonk", "search", "--semantic", "verifyToken"]).unwrap();
        match cli.command {
            Command::Search(args) => {
                assert_eq!(args.pattern, "verifyToken");
                assert!(args.semantic);
                assert!(!args.raw);
            }
            _ => panic!("expected Command::Search"),
        }
    }

    #[test]
    fn parse_search_semantic_default_false() {
        let cli = Cli::try_parse_from(["wonk", "search", "verifyToken"]).unwrap();
        match cli.command {
            Command::Search(args) => {
                assert!(!args.semantic);
            }
            _ => panic!("expected Command::Search"),
        }
    }

    #[test]
    fn parse_search_semantic_conflicts_with_raw() {
        let result = Cli::try_parse_from(["wonk", "search", "--semantic", "--raw", "pattern"]);
        assert!(result.is_err(), "--semantic and --raw should conflict");
    }

    #[test]
    fn parse_cluster_basic() {
        let cli = Cli::try_parse_from(["wonk", "cluster", "src/auth/"]).unwrap();
        match cli.command {
            Command::Cluster(args) => {
                assert_eq!(args.path, "src/auth/");
                assert_eq!(args.top, 5);
            }
            _ => panic!("expected Command::Cluster"),
        }
    }

    #[test]
    fn parse_cluster_with_top() {
        let cli = Cli::try_parse_from(["wonk", "cluster", "--top", "10", "src/auth/"]).unwrap();
        match cli.command {
            Command::Cluster(args) => {
                assert_eq!(args.path, "src/auth/");
                assert_eq!(args.top, 10);
            }
            _ => panic!("expected Command::Cluster"),
        }
    }

    #[test]
    fn parse_cluster_requires_path() {
        let result = Cli::try_parse_from(["wonk", "cluster"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_impact_basic() {
        let cli = Cli::try_parse_from(["wonk", "impact", "src/auth/middleware.ts"]).unwrap();
        match cli.command {
            Command::Impact(args) => {
                assert_eq!(args.file, "src/auth/middleware.ts");
                assert!(args.since.is_none());
            }
            _ => panic!("expected Command::Impact"),
        }
    }

    #[test]
    fn parse_impact_with_since() {
        let cli = Cli::try_parse_from([
            "wonk",
            "impact",
            "--since",
            "HEAD~3",
            "src/auth/middleware.ts",
        ])
        .unwrap();
        match cli.command {
            Command::Impact(args) => {
                assert_eq!(args.file, "src/auth/middleware.ts");
                assert_eq!(args.since.as_deref(), Some("HEAD~3"));
            }
            _ => panic!("expected Command::Impact"),
        }
    }

    #[test]
    fn parse_impact_requires_file() {
        let result = Cli::try_parse_from(["wonk", "impact"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_impact_with_global_json() {
        let cli =
            Cli::try_parse_from(["wonk", "--format", "json", "impact", "src/main.rs"]).unwrap();
        assert_eq!(cli.format, Some(OutputFormat::Json));
        match cli.command {
            Command::Impact(args) => {
                assert_eq!(args.file, "src/main.rs");
            }
            _ => panic!("expected Command::Impact"),
        }
    }

    #[test]
    fn parse_show_basic() {
        let cli = Cli::try_parse_from(["wonk", "show", "processPayment"]).unwrap();
        match cli.command {
            Command::Show(args) => {
                assert_eq!(args.name, "processPayment");
                assert!(args.file.is_none());
                assert!(args.kind.is_none());
                assert!(!args.exact);
            }
            _ => panic!("expected Command::Show"),
        }
    }

    #[test]
    fn parse_show_with_file() {
        let cli =
            Cli::try_parse_from(["wonk", "show", "--file", "src/billing.ts", "processPayment"])
                .unwrap();
        match cli.command {
            Command::Show(args) => {
                assert_eq!(args.name, "processPayment");
                assert_eq!(args.file.as_deref(), Some("src/billing.ts"));
            }
            _ => panic!("expected Command::Show"),
        }
    }

    #[test]
    fn parse_show_with_kind() {
        let cli =
            Cli::try_parse_from(["wonk", "show", "--kind", "function", "processPayment"]).unwrap();
        match cli.command {
            Command::Show(args) => {
                assert_eq!(args.kind.as_deref(), Some("function"));
            }
            _ => panic!("expected Command::Show"),
        }
    }

    #[test]
    fn parse_show_with_exact() {
        let cli = Cli::try_parse_from(["wonk", "show", "--exact", "processPayment"]).unwrap();
        match cli.command {
            Command::Show(args) => {
                assert!(args.exact);
            }
            _ => panic!("expected Command::Show"),
        }
    }

    #[test]
    fn parse_show_requires_name() {
        let result = Cli::try_parse_from(["wonk", "show"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_show_with_global_format() {
        let cli =
            Cli::try_parse_from(["wonk", "--format", "json", "show", "processPayment"]).unwrap();
        assert_eq!(cli.format, Some(OutputFormat::Json));
        match cli.command {
            Command::Show(args) => {
                assert_eq!(args.name, "processPayment");
            }
            _ => panic!("expected Command::Show"),
        }
    }

    #[test]
    fn parse_cluster_with_global_budget() {
        let cli = Cli::try_parse_from(["wonk", "--budget", "500", "cluster", "src/auth/"]).unwrap();
        assert_eq!(cli.budget, Some(500));
        match cli.command {
            Command::Cluster(args) => {
                assert_eq!(args.path, "src/auth/");
            }
            _ => panic!("expected Command::Cluster"),
        }
    }
}
