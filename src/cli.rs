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
    Update(UpdateArgs),

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

    /// Find all callers of a symbol (functions whose bodies reference it)
    Callers(CallersArgs),

    /// Find all callees of a symbol (symbols referenced within its body)
    Callees(CalleesArgs),

    /// Find a call chain between two symbols
    Callpath(CallpathArgs),

    /// Show a structural summary of a file or directory
    Summary(SummaryArgs),

    /// Detect entry points and trace execution flows
    Flows(FlowsArgs),

    /// Analyze blast radius of a symbol change
    Blast(BlastArgs),

    /// Detect changed symbols and optionally chain blast/flow analysis
    Changes(ChangesArgs),

    /// Aggregate full context for a symbol: definition, callers, callees, importers, flows, children
    Context(ContextArgs),
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
pub struct UpdateArgs {
    /// Force a full rebuild even if the index appears current
    #[arg(long)]
    pub force: bool,
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

    /// Show container types (class, struct, enum, trait, interface) in shallow
    /// mode: signature + child signatures without bodies
    #[arg(long)]
    pub shallow: bool,
}

#[derive(clap::Args, Debug)]
pub struct CallersArgs {
    /// Symbol name to find callers for
    pub name: String,

    /// Transitive expansion depth (default: 1 = direct callers only, max: 10)
    #[arg(long, default_value_t = 1)]
    pub depth: usize,

    /// Minimum confidence threshold (0.0-1.0) to filter results
    #[arg(long)]
    pub min_confidence: Option<f64>,
}

#[derive(clap::Args, Debug)]
pub struct CalleesArgs {
    /// Symbol name to find callees for
    pub name: String,

    /// Transitive expansion depth (default: 1 = direct callees only, max: 10)
    #[arg(long, default_value_t = 1)]
    pub depth: usize,

    /// Minimum confidence threshold (0.0-1.0) to filter results
    #[arg(long)]
    pub min_confidence: Option<f64>,
}

#[derive(clap::Args, Debug)]
pub struct CallpathArgs {
    /// Starting symbol name
    pub from: String,
    /// Target symbol name
    pub to: String,

    /// Minimum confidence threshold (0.0-1.0) to filter edges
    #[arg(long)]
    pub min_confidence: Option<f64>,
}

#[derive(clap::Args, Debug)]
pub struct SummaryArgs {
    /// Path to summarize (file or directory)
    pub path: String,

    /// Detail level: rich (default), light, or symbols
    #[arg(long, default_value = "rich")]
    pub detail: String,

    /// Recursion depth for child summaries (0 = target only)
    #[arg(long, default_value_t = 0)]
    pub depth: usize,

    /// Show full recursive hierarchy (unlimited depth)
    #[arg(long, conflicts_with = "depth")]
    pub recursive: bool,

    /// Include AI-generated description (requires embeddings)
    #[arg(long)]
    pub semantic: bool,
}

#[derive(clap::Args, Debug)]
pub struct FlowsArgs {
    /// Entry point name to trace (omit to list all detected entry points)
    pub entry: Option<String>,

    /// Restrict entry point detection to symbols in this file
    #[arg(long)]
    pub from: Option<String>,

    /// Maximum BFS traversal depth (default: 10, max: 20)
    #[arg(long, default_value_t = 10)]
    pub depth: usize,

    /// Maximum callees to follow per symbol (default: 4)
    #[arg(long, default_value_t = 4)]
    pub branching: usize,

    /// Minimum confidence threshold (0.0-1.0) to filter callee edges
    #[arg(long)]
    pub min_confidence: Option<f64>,
}

#[derive(clap::Args, Debug)]
pub struct BlastArgs {
    /// Symbol name to analyze blast radius for
    pub symbol: String,

    /// Traversal direction: upstream (default) or downstream
    #[arg(long)]
    pub direction: Option<String>,

    /// Maximum traversal depth (default: 3, max: 10)
    #[arg(long, default_value_t = 3)]
    pub depth: usize,

    /// Include test files in results
    #[arg(long)]
    pub include_tests: bool,

    /// Minimum confidence threshold (0.0-1.0) to filter edges
    #[arg(long)]
    pub min_confidence: Option<f64>,
}

#[derive(clap::Args, Debug)]
pub struct ChangesArgs {
    /// Change scope: unstaged (default), staged, all, or compare
    #[arg(long, default_value = "unstaged")]
    pub scope: String,

    /// Base git ref for compare scope (required when --scope=compare)
    #[arg(long)]
    pub base: Option<String>,

    /// Include blast radius analysis for each changed symbol
    #[arg(long)]
    pub blast: bool,

    /// Identify execution flows affected by changed symbols
    #[arg(long)]
    pub flows: bool,

    /// Minimum confidence threshold (0.0-1.0) for blast/flow edges
    #[arg(long)]
    pub min_confidence: Option<f64>,
}

#[derive(clap::Args, Debug)]
pub struct ContextArgs {
    /// Symbol name to look up
    pub name: String,

    /// Restrict to symbols in this file
    #[arg(long)]
    pub file: Option<String>,

    /// Filter by symbol kind (e.g. function, class)
    #[arg(long)]
    pub kind: Option<String>,

    /// Minimum confidence threshold (0.0-1.0) to filter edges
    #[arg(long)]
    pub min_confidence: Option<f64>,
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
    fn parse_show_with_shallow() {
        let cli = Cli::try_parse_from(["wonk", "show", "--shallow", "MyClass"]).unwrap();
        match cli.command {
            Command::Show(args) => {
                assert_eq!(args.name, "MyClass");
                assert!(args.shallow);
            }
            _ => panic!("expected Command::Show"),
        }
    }

    #[test]
    fn parse_show_shallow_default_false() {
        let cli = Cli::try_parse_from(["wonk", "show", "MyClass"]).unwrap();
        match cli.command {
            Command::Show(args) => {
                assert!(!args.shallow);
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

    // -- Callers/Callees tests -----------------------------------------------

    #[test]
    fn parse_callers_basic() {
        let cli = Cli::try_parse_from(["wonk", "callers", "dispatch"]).unwrap();
        match cli.command {
            Command::Callers(args) => {
                assert_eq!(args.name, "dispatch");
                assert_eq!(args.depth, 1);
            }
            _ => panic!("expected Command::Callers"),
        }
    }

    #[test]
    fn parse_callers_with_depth() {
        let cli = Cli::try_parse_from(["wonk", "callers", "--depth", "3", "dispatch"]).unwrap();
        match cli.command {
            Command::Callers(args) => {
                assert_eq!(args.name, "dispatch");
                assert_eq!(args.depth, 3);
            }
            _ => panic!("expected Command::Callers"),
        }
    }

    #[test]
    fn parse_callers_requires_name() {
        let result = Cli::try_parse_from(["wonk", "callers"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_callees_basic() {
        let cli = Cli::try_parse_from(["wonk", "callees", "main"]).unwrap();
        match cli.command {
            Command::Callees(args) => {
                assert_eq!(args.name, "main");
                assert_eq!(args.depth, 1);
            }
            _ => panic!("expected Command::Callees"),
        }
    }

    #[test]
    fn parse_callees_with_depth() {
        let cli = Cli::try_parse_from(["wonk", "callees", "--depth", "5", "main"]).unwrap();
        match cli.command {
            Command::Callees(args) => {
                assert_eq!(args.name, "main");
                assert_eq!(args.depth, 5);
            }
            _ => panic!("expected Command::Callees"),
        }
    }

    #[test]
    fn parse_callees_requires_name() {
        let result = Cli::try_parse_from(["wonk", "callees"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_callers_with_global_json() {
        let cli = Cli::try_parse_from(["wonk", "--format", "json", "callers", "dispatch"]).unwrap();
        assert_eq!(cli.format, Some(OutputFormat::Json));
        match cli.command {
            Command::Callers(args) => {
                assert_eq!(args.name, "dispatch");
            }
            _ => panic!("expected Command::Callers"),
        }
    }

    #[test]
    fn parse_callers_with_global_budget() {
        let cli = Cli::try_parse_from(["wonk", "--budget", "500", "callers", "dispatch"]).unwrap();
        assert_eq!(cli.budget, Some(500));
        match cli.command {
            Command::Callers(args) => {
                assert_eq!(args.name, "dispatch");
            }
            _ => panic!("expected Command::Callers"),
        }
    }

    #[test]
    fn parse_callers_with_min_confidence() {
        let cli = Cli::try_parse_from(["wonk", "callers", "--min-confidence", "0.8", "dispatch"])
            .unwrap();
        match cli.command {
            Command::Callers(args) => {
                assert_eq!(args.name, "dispatch");
                assert_eq!(args.min_confidence, Some(0.8));
            }
            _ => panic!("expected Command::Callers"),
        }
    }

    #[test]
    fn parse_callers_min_confidence_default_none() {
        let cli = Cli::try_parse_from(["wonk", "callers", "dispatch"]).unwrap();
        match cli.command {
            Command::Callers(args) => {
                assert!(args.min_confidence.is_none());
            }
            _ => panic!("expected Command::Callers"),
        }
    }

    #[test]
    fn parse_callees_with_min_confidence() {
        let cli =
            Cli::try_parse_from(["wonk", "callees", "--min-confidence", "0.9", "main"]).unwrap();
        match cli.command {
            Command::Callees(args) => {
                assert_eq!(args.min_confidence, Some(0.9));
            }
            _ => panic!("expected Command::Callees"),
        }
    }

    #[test]
    fn parse_callpath_with_min_confidence() {
        let cli = Cli::try_parse_from([
            "wonk",
            "callpath",
            "--min-confidence",
            "0.7",
            "main",
            "dispatch",
        ])
        .unwrap();
        match cli.command {
            Command::Callpath(args) => {
                assert_eq!(args.min_confidence, Some(0.7));
            }
            _ => panic!("expected Command::Callpath"),
        }
    }

    // -- Callpath tests -------------------------------------------------------

    #[test]
    fn parse_callpath_basic() {
        let cli = Cli::try_parse_from(["wonk", "callpath", "main", "dispatch"]).unwrap();
        match cli.command {
            Command::Callpath(args) => {
                assert_eq!(args.from, "main");
                assert_eq!(args.to, "dispatch");
            }
            _ => panic!("expected Command::Callpath"),
        }
    }

    #[test]
    fn parse_callpath_with_format() {
        let cli =
            Cli::try_parse_from(["wonk", "--format", "json", "callpath", "foo", "bar"]).unwrap();
        assert_eq!(cli.format, Some(OutputFormat::Json));
        match cli.command {
            Command::Callpath(args) => {
                assert_eq!(args.from, "foo");
                assert_eq!(args.to, "bar");
            }
            _ => panic!("expected Command::Callpath"),
        }
    }

    #[test]
    fn parse_callpath_requires_both_args() {
        let result = Cli::try_parse_from(["wonk", "callpath", "foo"]);
        assert!(result.is_err(), "callpath requires both from and to");
    }

    // -- Summary tests --------------------------------------------------------

    #[test]
    fn parse_summary_basic() {
        let cli = Cli::try_parse_from(["wonk", "summary", "src/"]).unwrap();
        match cli.command {
            Command::Summary(args) => {
                assert_eq!(args.path, "src/");
                assert_eq!(args.detail, "rich");
                assert_eq!(args.depth, 0);
                assert!(!args.recursive);
                assert!(!args.semantic);
            }
            _ => panic!("expected Command::Summary"),
        }
    }

    #[test]
    fn parse_summary_with_detail() {
        let cli = Cli::try_parse_from(["wonk", "summary", "--detail", "light", "src/"]).unwrap();
        match cli.command {
            Command::Summary(args) => {
                assert_eq!(args.detail, "light");
            }
            _ => panic!("expected Command::Summary"),
        }
    }

    #[test]
    fn parse_summary_with_depth() {
        let cli = Cli::try_parse_from(["wonk", "summary", "--depth", "2", "src/"]).unwrap();
        match cli.command {
            Command::Summary(args) => {
                assert_eq!(args.depth, 2);
                assert!(!args.recursive);
            }
            _ => panic!("expected Command::Summary"),
        }
    }

    #[test]
    fn parse_summary_recursive() {
        let cli = Cli::try_parse_from(["wonk", "summary", "--recursive", "src/"]).unwrap();
        match cli.command {
            Command::Summary(args) => {
                assert!(args.recursive);
            }
            _ => panic!("expected Command::Summary"),
        }
    }

    #[test]
    fn parse_summary_recursive_conflicts_with_depth() {
        let result =
            Cli::try_parse_from(["wonk", "summary", "--recursive", "--depth", "2", "src/"]);
        assert!(result.is_err(), "--recursive and --depth should conflict");
    }

    #[test]
    fn parse_summary_semantic_flag() {
        let cli = Cli::try_parse_from(["wonk", "summary", "--semantic", "src/"]).unwrap();
        match cli.command {
            Command::Summary(args) => {
                assert!(args.semantic);
            }
            _ => panic!("expected Command::Summary"),
        }
    }

    #[test]
    fn parse_summary_requires_path() {
        let result = Cli::try_parse_from(["wonk", "summary"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_summary_with_global_format() {
        let cli = Cli::try_parse_from(["wonk", "--format", "json", "summary", "src/"]).unwrap();
        assert_eq!(cli.format, Some(OutputFormat::Json));
        match cli.command {
            Command::Summary(args) => {
                assert_eq!(args.path, "src/");
            }
            _ => panic!("expected Command::Summary"),
        }
    }

    #[test]
    fn parse_summary_with_global_budget() {
        let cli = Cli::try_parse_from(["wonk", "--budget", "500", "summary", "src/"]).unwrap();
        assert_eq!(cli.budget, Some(500));
        match cli.command {
            Command::Summary(args) => {
                assert_eq!(args.path, "src/");
            }
            _ => panic!("expected Command::Summary"),
        }
    }

    // -- Flows tests ----------------------------------------------------------

    #[test]
    fn parse_flows_no_args() {
        let cli = Cli::try_parse_from(["wonk", "flows"]).unwrap();
        match cli.command {
            Command::Flows(args) => {
                assert!(args.entry.is_none());
                assert!(args.from.is_none());
                assert_eq!(args.depth, 10);
                assert_eq!(args.branching, 4);
                assert!(args.min_confidence.is_none());
            }
            _ => panic!("expected Command::Flows"),
        }
    }

    #[test]
    fn parse_flows_with_entry() {
        let cli = Cli::try_parse_from(["wonk", "flows", "main"]).unwrap();
        match cli.command {
            Command::Flows(args) => {
                assert_eq!(args.entry.as_deref(), Some("main"));
            }
            _ => panic!("expected Command::Flows"),
        }
    }

    #[test]
    fn parse_flows_with_from() {
        let cli = Cli::try_parse_from(["wonk", "flows", "--from", "src/api.ts"]).unwrap();
        match cli.command {
            Command::Flows(args) => {
                assert!(args.entry.is_none());
                assert_eq!(args.from.as_deref(), Some("src/api.ts"));
            }
            _ => panic!("expected Command::Flows"),
        }
    }

    #[test]
    fn parse_flows_with_depth() {
        let cli = Cli::try_parse_from(["wonk", "flows", "--depth", "5", "main"]).unwrap();
        match cli.command {
            Command::Flows(args) => {
                assert_eq!(args.depth, 5);
                assert_eq!(args.entry.as_deref(), Some("main"));
            }
            _ => panic!("expected Command::Flows"),
        }
    }

    #[test]
    fn parse_flows_with_branching() {
        let cli = Cli::try_parse_from(["wonk", "flows", "--branching", "2", "main"]).unwrap();
        match cli.command {
            Command::Flows(args) => {
                assert_eq!(args.branching, 2);
            }
            _ => panic!("expected Command::Flows"),
        }
    }

    #[test]
    fn parse_flows_with_min_confidence() {
        let cli =
            Cli::try_parse_from(["wonk", "flows", "--min-confidence", "0.8", "main"]).unwrap();
        match cli.command {
            Command::Flows(args) => {
                assert_eq!(args.min_confidence, Some(0.8));
            }
            _ => panic!("expected Command::Flows"),
        }
    }

    #[test]
    fn parse_flows_with_global_format() {
        let cli = Cli::try_parse_from(["wonk", "--format", "json", "flows", "main"]).unwrap();
        assert_eq!(cli.format, Some(OutputFormat::Json));
        match cli.command {
            Command::Flows(args) => {
                assert_eq!(args.entry.as_deref(), Some("main"));
            }
            _ => panic!("expected Command::Flows"),
        }
    }

    #[test]
    fn parse_flows_with_global_budget() {
        let cli = Cli::try_parse_from(["wonk", "--budget", "500", "flows"]).unwrap();
        assert_eq!(cli.budget, Some(500));
        match cli.command {
            Command::Flows(_) => {}
            _ => panic!("expected Command::Flows"),
        }
    }

    // -- Blast tests ----------------------------------------------------------

    #[test]
    fn parse_blast_basic() {
        let cli = Cli::try_parse_from(["wonk", "blast", "processPayment"]).unwrap();
        match cli.command {
            Command::Blast(args) => {
                assert_eq!(args.symbol, "processPayment");
                assert!(args.direction.is_none());
                assert_eq!(args.depth, 3);
                assert!(!args.include_tests);
                assert!(args.min_confidence.is_none());
            }
            _ => panic!("expected Command::Blast"),
        }
    }

    #[test]
    fn parse_blast_with_direction() {
        let cli = Cli::try_parse_from([
            "wonk",
            "blast",
            "--direction",
            "downstream",
            "processPayment",
        ])
        .unwrap();
        match cli.command {
            Command::Blast(args) => {
                assert_eq!(args.direction.as_deref(), Some("downstream"));
            }
            _ => panic!("expected Command::Blast"),
        }
    }

    #[test]
    fn parse_blast_with_depth() {
        let cli = Cli::try_parse_from(["wonk", "blast", "--depth", "5", "processPayment"]).unwrap();
        match cli.command {
            Command::Blast(args) => {
                assert_eq!(args.depth, 5);
            }
            _ => panic!("expected Command::Blast"),
        }
    }

    #[test]
    fn parse_blast_with_include_tests() {
        let cli =
            Cli::try_parse_from(["wonk", "blast", "--include-tests", "processPayment"]).unwrap();
        match cli.command {
            Command::Blast(args) => {
                assert!(args.include_tests);
            }
            _ => panic!("expected Command::Blast"),
        }
    }

    #[test]
    fn parse_blast_with_min_confidence() {
        let cli =
            Cli::try_parse_from(["wonk", "blast", "--min-confidence", "0.8", "processPayment"])
                .unwrap();
        match cli.command {
            Command::Blast(args) => {
                assert_eq!(args.min_confidence, Some(0.8));
            }
            _ => panic!("expected Command::Blast"),
        }
    }

    #[test]
    fn parse_blast_requires_symbol() {
        let result = Cli::try_parse_from(["wonk", "blast"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_blast_with_global_format() {
        let cli =
            Cli::try_parse_from(["wonk", "--format", "json", "blast", "processPayment"]).unwrap();
        assert_eq!(cli.format, Some(OutputFormat::Json));
        match cli.command {
            Command::Blast(args) => {
                assert_eq!(args.symbol, "processPayment");
            }
            _ => panic!("expected Command::Blast"),
        }
    }

    #[test]
    fn parse_blast_with_global_budget() {
        let cli =
            Cli::try_parse_from(["wonk", "--budget", "500", "blast", "processPayment"]).unwrap();
        assert_eq!(cli.budget, Some(500));
        match cli.command {
            Command::Blast(args) => {
                assert_eq!(args.symbol, "processPayment");
            }
            _ => panic!("expected Command::Blast"),
        }
    }

    // -- Changes tests (TASK-072) ---------------------------------------------

    #[test]
    fn parse_changes_default() {
        let cli = Cli::try_parse_from(["wonk", "changes"]).unwrap();
        match cli.command {
            Command::Changes(args) => {
                assert_eq!(args.scope, "unstaged");
                assert!(args.base.is_none());
                assert!(!args.blast);
                assert!(!args.flows);
                assert!(args.min_confidence.is_none());
            }
            _ => panic!("expected Command::Changes"),
        }
    }

    #[test]
    fn parse_changes_scope_staged() {
        let cli = Cli::try_parse_from(["wonk", "changes", "--scope", "staged"]).unwrap();
        match cli.command {
            Command::Changes(args) => {
                assert_eq!(args.scope, "staged");
            }
            _ => panic!("expected Command::Changes"),
        }
    }

    #[test]
    fn parse_changes_scope_all() {
        let cli = Cli::try_parse_from(["wonk", "changes", "--scope", "all"]).unwrap();
        match cli.command {
            Command::Changes(args) => {
                assert_eq!(args.scope, "all");
            }
            _ => panic!("expected Command::Changes"),
        }
    }

    #[test]
    fn parse_changes_scope_compare_with_base() {
        let cli = Cli::try_parse_from(["wonk", "changes", "--scope", "compare", "--base", "main"])
            .unwrap();
        match cli.command {
            Command::Changes(args) => {
                assert_eq!(args.scope, "compare");
                assert_eq!(args.base.as_deref(), Some("main"));
            }
            _ => panic!("expected Command::Changes"),
        }
    }

    #[test]
    fn parse_changes_blast_flag() {
        let cli = Cli::try_parse_from(["wonk", "changes", "--blast"]).unwrap();
        match cli.command {
            Command::Changes(args) => {
                assert!(args.blast);
                assert!(!args.flows);
            }
            _ => panic!("expected Command::Changes"),
        }
    }

    #[test]
    fn parse_changes_flows_flag() {
        let cli = Cli::try_parse_from(["wonk", "changes", "--flows"]).unwrap();
        match cli.command {
            Command::Changes(args) => {
                assert!(!args.blast);
                assert!(args.flows);
            }
            _ => panic!("expected Command::Changes"),
        }
    }

    #[test]
    fn parse_changes_blast_and_flows() {
        let cli = Cli::try_parse_from(["wonk", "changes", "--blast", "--flows"]).unwrap();
        match cli.command {
            Command::Changes(args) => {
                assert!(args.blast);
                assert!(args.flows);
            }
            _ => panic!("expected Command::Changes"),
        }
    }

    #[test]
    fn parse_changes_min_confidence() {
        let cli = Cli::try_parse_from(["wonk", "changes", "--min-confidence", "0.8"]).unwrap();
        match cli.command {
            Command::Changes(args) => {
                assert_eq!(args.min_confidence, Some(0.8));
            }
            _ => panic!("expected Command::Changes"),
        }
    }

    // -- Context tests (TASK-073) ---------------------------------------------

    #[test]
    fn parse_context_basic() {
        let cli = Cli::try_parse_from(["wonk", "context", "processPayment"]).unwrap();
        match cli.command {
            Command::Context(args) => {
                assert_eq!(args.name, "processPayment");
                assert!(args.file.is_none());
                assert!(args.kind.is_none());
                assert!(args.min_confidence.is_none());
            }
            _ => panic!("expected Command::Context"),
        }
    }

    #[test]
    fn parse_context_with_file() {
        let cli = Cli::try_parse_from(["wonk", "context", "--file", "src/auth.ts", "verifyToken"])
            .unwrap();
        match cli.command {
            Command::Context(args) => {
                assert_eq!(args.name, "verifyToken");
                assert_eq!(args.file.as_deref(), Some("src/auth.ts"));
            }
            _ => panic!("expected Command::Context"),
        }
    }

    #[test]
    fn parse_context_with_kind() {
        let cli =
            Cli::try_parse_from(["wonk", "context", "--kind", "class", "StripeClient"]).unwrap();
        match cli.command {
            Command::Context(args) => {
                assert_eq!(args.kind.as_deref(), Some("class"));
            }
            _ => panic!("expected Command::Context"),
        }
    }

    #[test]
    fn parse_context_with_min_confidence() {
        let cli = Cli::try_parse_from([
            "wonk",
            "context",
            "--min-confidence",
            "0.8",
            "processPayment",
        ])
        .unwrap();
        match cli.command {
            Command::Context(args) => {
                assert_eq!(args.min_confidence, Some(0.8));
            }
            _ => panic!("expected Command::Context"),
        }
    }

    #[test]
    fn parse_context_requires_name() {
        let result = Cli::try_parse_from(["wonk", "context"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_context_with_global_format() {
        let cli =
            Cli::try_parse_from(["wonk", "--format", "json", "context", "processPayment"]).unwrap();
        assert_eq!(cli.format, Some(OutputFormat::Json));
        match cli.command {
            Command::Context(args) => {
                assert_eq!(args.name, "processPayment");
            }
            _ => panic!("expected Command::Context"),
        }
    }

    #[test]
    fn parse_context_with_global_budget() {
        let cli =
            Cli::try_parse_from(["wonk", "--budget", "500", "context", "processPayment"]).unwrap();
        assert_eq!(cli.budget, Some(500));
        match cli.command {
            Command::Context(args) => {
                assert_eq!(args.name, "processPayment");
            }
            _ => panic!("expected Command::Context"),
        }
    }
}
