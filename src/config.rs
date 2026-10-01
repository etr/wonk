//! Configuration file parsing, defaults, and merging.
//!
//! Configuration is loaded in layers (last wins):
//! 1. Built-in defaults
//! 2. Global config from `~/.wonk/config.toml`
//! 3. Per-repo config from `<repo_root>/.wonk/config.toml`
//!
//! Each layer only overrides fields it explicitly sets; absent fields
//! are left at their previous value.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::embedding::EmbeddingProviderKind;

// ---------------------------------------------------------------------------
// Public config types (fully resolved, no Options)
// ---------------------------------------------------------------------------

/// Top-level configuration, fully resolved with defaults applied.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub index: IndexConfig,
    pub output: OutputConfig,
    pub ignore: IgnoreConfig,
    pub llm: LlmConfig,
    pub search: SearchConfig,
    pub embedding: EmbeddingConfig,
    pub reach: ReachConfig,
    pub contracts: ContractsConfig,
    pub review: ReviewConfig,
    pub rank: RankConfig,
}

/// Daemon-related settings.
#[derive(Debug, Clone, PartialEq)]
pub struct DaemonConfig {
    /// Debounce interval in milliseconds for file-change events.
    pub debounce_ms: u64,
}

/// Indexing settings.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexConfig {
    /// Maximum file size (in KiB) that the indexer will process.
    pub max_file_size_kb: u64,
    /// Extra file extensions to index beyond the built-in set.
    pub additional_extensions: Vec<String>,
}

/// Output / display settings.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputConfig {
    /// Default output format: `"grep"`, `"json"`, or `"toon"`.
    pub default_format: String,
    /// Color mode: `"auto"`, `"always"`, or `"never"`.
    pub color: String,
}

/// Ignore / exclusion settings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IgnoreConfig {
    /// Extra glob patterns to exclude from walks and indexing.
    pub patterns: Vec<String>,
}

/// LLM generation settings (for `--semantic` descriptions).
#[derive(Debug, Clone, PartialEq)]
pub struct LlmConfig {
    /// Ollama model name for text generation.
    pub model: String,
    /// Full URL for the Ollama `/api/generate` endpoint.
    pub generate_url: String,
}

/// Search-related settings.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchConfig {
    /// Reciprocal Rank Fusion constant K.
    ///
    /// Controls how much weight is given to rank position when fusing
    /// structural and semantic result lists. Higher values produce more
    /// even blending. Default: 60.0 (standard RRF constant).
    pub rrf_k: f32,
    /// BM25 term-frequency saturation strength (k1).
    ///
    /// Higher values let term frequency keep contributing longer before
    /// saturating. Default: 1.2 (Lucene's default).
    pub bm25_k1: f32,
    /// BM25 length-normalization strength (b), in `[0, 1]`.
    ///
    /// 0 disables length normalization; 1 fully normalizes by document
    /// length. Default: 0.75 (Lucene's default).
    pub bm25_b: f32,
}

/// Embedding provider selection.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EmbeddingConfig {
    pub provider: EmbeddingProviderKind,
}

/// Precomputed reach index settings (TASK-080, DR-034).
#[derive(Debug, Clone, PartialEq)]
pub struct ReachConfig {
    /// Depth to which the reach table is materialized during index build.
    /// Clamped to `blast::MAX_DEPTH` at the use site.
    pub depth: usize,
    /// Kill switch: `false` skips the build and restores exact V4 behavior
    /// (PRD-REACH-REQ-006).
    pub enabled: bool,
}

impl Default for ReachConfig {
    fn default() -> Self {
        Self {
            depth: 3,
            enabled: true,
        }
    }
}

/// Review rule-family switches (TASK-085, AR-022).
///
/// Per-family booleans rather than a rules list: each family layers
/// independently, and a noisy rule can be silenced alone pending OQ-013
/// calibration. All default to enabled (cross-repo arrives TASK-086).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewConfig {
    /// Rule family A: breaking change.
    pub breaking_change: bool,
    /// Rule family B: coverage gap.
    pub coverage_gap: bool,
    /// Rule family C: cross-repo contract impact (TASK-086).
    pub cross_repo: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            breaking_change: true,
            coverage_gap: true,
            cross_repo: true,
        }
    }
}

/// Contract-extraction kind switches (TASK-087, PRD-CTR-REQ-001).
///
/// Per-kind booleans rather than a kinds list: each kind layers
/// independently, and a noisy detector can be turned off without touching
/// the others. All kinds default to enabled; TASK-088 adds the RPC-family
/// booleans, and workspace scoping arrives with TASK-083+.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractsConfig {
    /// HTTP route/outbound-call detection.
    pub http: bool,
    /// Environment-variable read/write detection.
    pub env: bool,
    /// Message-queue producer/consumer detection.
    pub queue: bool,
    /// WebSocket emit/handler-registration detection.
    pub websocket: bool,
    /// Scheduled and background job detection.
    pub job: bool,
    /// gRPC IDL + generated-stub detection (RPC family, TASK-088).
    pub grpc: bool,
    /// GraphQL resolver + operation detection (TASK-088).
    pub graphql: bool,
    /// OpenAPI specification-document detection (TASK-088).
    pub openapi: bool,
    /// Workspace identifiers this repo declares (TASK-084, PRD-CTR-REQ-013).
    ///
    /// Repo-local only — the value is read from `<repo>/.wonk/config.toml`;
    /// a global-layer value is ignored with a warning (PRD-CTR-REQ-017).
    /// Stored verbatim (trimming/case-folding happens at comparison).
    pub workspace: Vec<String>,
}

impl Default for ContractsConfig {
    fn default() -> Self {
        Self {
            http: true,
            env: true,
            queue: true,
            websocket: true,
            job: true,
            grpc: true,
            graphql: true,
            openapi: true,
            workspace: Vec::new(),
        }
    }
}

/// Signal-pipeline reranking settings (TASK-092, PRD-RANK-REQ-006/017).
///
/// `enabled` gates the rerank pipeline: `false` (the default) keeps the
/// byte-identical legacy ordering; flipping the default is TASK-095's.
/// `weights` maps signal names to f32 multipliers and is validated against
/// the signal registry at load time — the first config key whose unknown
/// values are a hard load error rather than a silent no-op.
#[derive(Debug, Clone, PartialEq)]
pub struct RankConfig {
    /// Whether search results are reranked through the signal pipeline.
    pub enabled: bool,
    /// Signal name -> weight. Absent names weigh zero.
    pub weights: HashMap<String, f32>,
    /// Per-class scaling of the lexical/semantic channels (TASK-095,
    /// REQ-008). Neutral by default; the conceptual class has NO entry and
    /// cannot acquire one (REQ-009 — the parser hard-rejects it).
    pub class_multipliers: crate::rerank::ClassMultipliers,
}

impl Default for RankConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            weights: HashMap::from([("kind".to_string(), 1.0)]),
            class_multipliers: crate::rerank::ClassMultipliers::neutral(),
        }
    }
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

impl Default for DaemonConfig {
    fn default() -> Self {
        Self { debounce_ms: 500 }
    }
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            max_file_size_kb: 1024,
            additional_extensions: Vec::new(),
        }
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            default_format: "grep".to_string(),
            color: "auto".to_string(),
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: "llama3.2:3b".to_string(),
            generate_url: "http://localhost:11434/api/generate".to_string(),
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            rrf_k: 60.0,
            bm25_k1: 1.2,
            bm25_b: 0.75,
        }
    }
}

// ---------------------------------------------------------------------------
// Option-based overlay types (for partial deserialization)
// ---------------------------------------------------------------------------

/// Mirror of [`Config`] where every field is `Option`, so we can
/// deserialize a partial TOML file and overlay only the keys that are
/// present.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ConfigOverlay {
    daemon: Option<DaemonOverlay>,
    index: Option<IndexOverlay>,
    output: Option<OutputOverlay>,
    ignore: Option<IgnoreOverlay>,
    llm: Option<LlmOverlay>,
    search: Option<SearchOverlay>,
    embedding: Option<EmbeddingOverlay>,
    reach: Option<ReachOverlay>,
    contracts: Option<ContractsOverlay>,
    review: Option<ReviewOverlay>,
    rank: Option<RankOverlay>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct DaemonOverlay {
    debounce_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct IndexOverlay {
    max_file_size_kb: Option<u64>,
    additional_extensions: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct OutputOverlay {
    default_format: Option<String>,
    color: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct IgnoreOverlay {
    patterns: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct LlmOverlay {
    model: Option<String>,
    generate_url: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct SearchOverlay {
    rrf_k: Option<f32>,
    bm25_k1: Option<f32>,
    bm25_b: Option<f32>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct EmbeddingOverlay {
    provider: Option<EmbeddingProviderKind>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ReachOverlay {
    depth: Option<usize>,
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ReviewOverlay {
    breaking_change: Option<bool>,
    coverage_gap: Option<bool>,
    cross_repo: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RankOverlay {
    enabled: Option<bool>,
    weights: Option<HashMap<String, f32>>,
    class_multipliers: Option<HashMap<String, ChannelMultipliersOverlay>>,
}

/// One `[rank.class_multipliers.<class>]` table; absent channels default
/// to the neutral 1.0.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ChannelMultipliersOverlay {
    lexical: Option<f32>,
    semantic: Option<f32>,
}

/// Validate a parsed `[rank.class_multipliers]` map (TASK-095). Unknown
/// classes and the deliberately-absent `conceptual` class are hard errors
/// (REQ-009); multipliers must be finite and >= 0.
fn validate_class_multipliers(
    raw: &HashMap<String, ChannelMultipliersOverlay>,
) -> Result<crate::rerank::ClassMultipliers> {
    let mut out = crate::rerank::ClassMultipliers::neutral();
    for (class, channel) in raw {
        let target = match class.as_str() {
            "symbol" => &mut out.symbol,
            "path" => &mut out.path,
            "signature" => &mut out.signature,
            "conceptual" => anyhow::bail!(
                "[rank.class_multipliers.conceptual] is rejected: the conceptual \
                 class is the neutral 1.0 baseline (PRD-RANK-REQ-009) and cannot \
                 be configured"
            ),
            other => anyhow::bail!(
                "unknown query class '{other}' in [rank.class_multipliers] \
                 (known: symbol, path, signature)"
            ),
        };
        for (name, value) in [("lexical", channel.lexical), ("semantic", channel.semantic)] {
            if let Some(v) = value
                && (!v.is_finite() || v < 0.0)
            {
                anyhow::bail!(
                    "class multiplier {v} for '{class}.{name}' in \
                     [rank.class_multipliers] must be finite and >= 0"
                );
            }
            match name {
                "lexical" => target.lexical = channel.lexical.unwrap_or(1.0),
                _ => target.semantic = channel.semantic.unwrap_or(1.0),
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ContractsOverlay {
    http: Option<bool>,
    env: Option<bool>,
    queue: Option<bool>,
    websocket: Option<bool>,
    job: Option<bool>,
    grpc: Option<bool>,
    graphql: Option<bool>,
    openapi: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    workspace: Option<Vec<String>>,
}

/// Deserialize a TOML value that may be a bare string or an array of
/// strings into `Vec<String>` (PRD-CTR-REQ-018).
fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum StringOrVec {
        One(String),
        Many(Vec<String>),
    }

    let value = Option::<StringOrVec>::deserialize(deserializer)?;
    Ok(value.map(|v| match v {
        StringOrVec::One(s) => vec![s],
        StringOrVec::Many(v) => v,
    }))
}

// ---------------------------------------------------------------------------
// Merge helpers
// ---------------------------------------------------------------------------

/// Which config layer an overlay came from (TASK-084, PRD-CTR-REQ-017).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLayer {
    /// `~/.wonk/config.toml` — `contracts.workspace` is ignored here.
    Global,
    /// `<repo>/.wonk/config.toml` — the only layer `contracts.workspace`
    /// is honored in.
    Repo,
}

impl Config {
    /// Apply an overlay on top of this config, replacing only the fields
    /// that are `Some` in the overlay. `layer` decides repo-local-only
    /// keys; ignored keys push a warning instead.
    fn apply_overlay(
        &mut self,
        overlay: ConfigOverlay,
        layer: ConfigLayer,
        warnings: &mut Vec<String>,
    ) -> Result<()> {
        if let Some(d) = overlay.daemon
            && let Some(v) = d.debounce_ms
        {
            self.daemon.debounce_ms = v;
        }
        if let Some(idx) = overlay.index {
            if let Some(v) = idx.max_file_size_kb {
                self.index.max_file_size_kb = v;
            }
            if let Some(v) = idx.additional_extensions {
                self.index.additional_extensions = v;
            }
        }
        if let Some(out) = overlay.output {
            if let Some(v) = out.default_format {
                self.output.default_format = v;
            }
            if let Some(v) = out.color {
                self.output.color = v;
            }
        }
        if let Some(ign) = overlay.ignore
            && let Some(v) = ign.patterns
        {
            self.ignore.patterns = v;
        }
        if let Some(llm) = overlay.llm {
            if let Some(v) = llm.model {
                self.llm.model = v;
            }
            if let Some(v) = llm.generate_url {
                self.llm.generate_url = v;
            }
        }
        if let Some(s) = overlay.search {
            if let Some(v) = s.rrf_k {
                self.search.rrf_k = v;
            }
            if let Some(v) = s.bm25_k1 {
                self.search.bm25_k1 = v;
            }
            if let Some(v) = s.bm25_b {
                self.search.bm25_b = v;
            }
        }
        if let Some(embedding) = overlay.embedding
            && let Some(provider) = embedding.provider
        {
            self.embedding.provider = provider;
        }
        if let Some(reach) = overlay.reach {
            if let Some(v) = reach.depth {
                self.reach.depth = v;
            }
            if let Some(v) = reach.enabled {
                self.reach.enabled = v;
            }
        }
        if let Some(review) = overlay.review {
            if let Some(v) = review.breaking_change {
                self.review.breaking_change = v;
            }
            if let Some(v) = review.coverage_gap {
                self.review.coverage_gap = v;
            }
            if let Some(v) = review.cross_repo {
                self.review.cross_repo = v;
            }
        }
        if let Some(contracts) = overlay.contracts {
            if let Some(v) = contracts.workspace {
                match layer {
                    ConfigLayer::Global => warnings.push(
                        "[contracts] workspace is repo-local only; the value in global config \
                         is ignored (declare it in <repo>/.wonk/config.toml)"
                            .to_string(),
                    ),
                    ConfigLayer::Repo => self.contracts.workspace = v,
                }
            }
            if let Some(v) = contracts.http {
                self.contracts.http = v;
            }
            if let Some(v) = contracts.env {
                self.contracts.env = v;
            }
            if let Some(v) = contracts.queue {
                self.contracts.queue = v;
            }
            if let Some(v) = contracts.websocket {
                self.contracts.websocket = v;
            }
            if let Some(v) = contracts.job {
                self.contracts.job = v;
            }
            if let Some(v) = contracts.grpc {
                self.contracts.grpc = v;
            }
            if let Some(v) = contracts.graphql {
                self.contracts.graphql = v;
            }
            if let Some(v) = contracts.openapi {
                self.contracts.openapi = v;
            }
        }
        if let Some(rank) = overlay.rank {
            if let Some(v) = rank.enabled {
                self.rank.enabled = v;
            }
            if let Some(v) = rank.weights {
                // Hard validation against the signal registry (REQ-006):
                // building the WeightTable rejects unknown names and
                // non-finite values as load errors.
                crate::rerank::WeightTable::from_config(&v)?;
                // The table replaces the previous layer's wholesale.
                self.rank.weights = v;
            }
            if let Some(v) = rank.class_multipliers {
                // Hard validation (REQ-008/009): unknown classes, the
                // unconfigurable conceptual class, and negative or
                // non-finite multipliers are load errors.
                self.rank.class_multipliers = validate_class_multipliers(&v)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Return the user's home directory.
pub(crate) fn home_dir() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir()
}

/// Parse a TOML string into a [`ConfigOverlay`], producing a clear error
/// message on malformed input.
fn parse_overlay(contents: &str, path: &Path) -> Result<ConfigOverlay> {
    toml::from_str(contents)
        .with_context(|| format!("failed to parse config file: {}", path.display()))
}

/// Try to read a config file and parse it as an overlay.
/// Returns `Ok(None)` if the file does not exist.
fn load_overlay(path: &Path) -> Result<Option<ConfigOverlay>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let overlay = parse_overlay(&contents, path)?;
            Ok(Some(overlay))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!(
            "failed to read config file {}: {}",
            path.display(),
            e
        )),
    }
}

impl Config {
    /// Load configuration by merging layers:
    /// defaults -> global (`~/.wonk/config.toml`) -> per-repo (`<repo>/.wonk/config.toml`).
    ///
    /// If `repo_root` is `None`, only the global config (if any) is applied
    /// on top of defaults.
    pub fn load(repo_root: Option<&Path>) -> Result<Config> {
        let global_dir = home_dir().map(|h| h.join(".wonk"));
        let (config, warnings) = Self::load_with_warnings(global_dir.as_deref(), repo_root)?;
        for warning in &warnings {
            crate::output::print_warning(warning);
        }
        Ok(config)
    }

    /// Internal: load config with an explicit global config directory,
    /// discarding layer warnings (kept for existing test callers).
    #[cfg(test)]
    fn load_with_global_dir(global_dir: Option<&Path>, repo_root: Option<&Path>) -> Result<Config> {
        Self::load_with_warnings(global_dir, repo_root).map(|(config, _)| config)
    }

    /// Internal: load config with explicit layers, collecting warnings so
    /// callers (and tests) can surface them without capturing stderr.
    fn load_with_warnings(
        global_dir: Option<&Path>,
        repo_root: Option<&Path>,
    ) -> Result<(Config, Vec<String>)> {
        let mut config = Config::default();
        let mut warnings = Vec::new();

        // Layer 2: global config
        if let Some(dir) = global_dir {
            let global_path = dir.join("config.toml");
            if let Some(overlay) = load_overlay(&global_path)? {
                config.apply_overlay(overlay, ConfigLayer::Global, &mut warnings)?;
            }
        }

        // Layer 3: per-repo config
        if let Some(root) = repo_root {
            let repo_config_path = root.join(".wonk").join("config.toml");
            if let Some(overlay) = load_overlay(&repo_config_path)? {
                config.apply_overlay(overlay, ConfigLayer::Repo, &mut warnings)?;
            }
        }

        Ok((config, warnings))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper to create temporary directories for global and/or per-repo
    /// configs.  Does NOT touch environment variables, so tests are safe
    /// to run in parallel.
    struct TestEnv {
        _global_dir: tempfile::TempDir,
        _repo_dir: Option<tempfile::TempDir>,
        global_path: PathBuf,
        repo_path: Option<PathBuf>,
    }

    impl TestEnv {
        fn new() -> Self {
            let global = tempfile::tempdir().unwrap();
            let global_path = global.path().to_path_buf();
            Self {
                _global_dir: global,
                _repo_dir: None,
                global_path,
                repo_path: None,
            }
        }

        /// Write a global config file at `<global_dir>/config.toml`.
        fn write_global_config(&self, toml_content: &str) {
            fs::write(self.global_path.join("config.toml"), toml_content).unwrap();
        }

        /// Create and return a temporary repo directory.
        fn create_repo(&mut self) -> PathBuf {
            let repo = tempfile::tempdir().unwrap();
            let path = repo.path().to_path_buf();
            self._repo_dir = Some(repo);
            self.repo_path = Some(path.clone());
            path
        }

        /// Write a per-repo config at `<repo>/.wonk/config.toml`.
        fn write_repo_config(&self, toml_content: &str) {
            let repo = self.repo_path.as_ref().expect("call create_repo first");
            let dir = repo.join(".wonk");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("config.toml"), toml_content).unwrap();
        }

        /// Load config using this test environment's directories.
        fn load(&self) -> Result<Config> {
            Config::load_with_global_dir(Some(&self.global_path), self.repo_path.as_deref())
        }

        /// Load config, also returning the collected layer warnings.
        fn load_with_warnings(&self) -> Result<(Config, Vec<String>)> {
            Config::load_with_warnings(Some(&self.global_path), self.repo_path.as_deref())
        }
    }

    #[test]
    fn workspace_string_parses() {
        let mut env = TestEnv::new();
        env.create_repo();
        env.write_repo_config(
            r#"
[contracts]
workspace = "payments"
"#,
        );
        let (config, warnings) = env.load_with_warnings().unwrap();
        assert_eq!(config.contracts.workspace, vec!["payments".to_string()]);
        assert!(warnings.is_empty(), "repo-layer workspace never warns");
    }

    #[test]
    fn workspace_array_parses() {
        let mut env = TestEnv::new();
        env.create_repo();
        env.write_repo_config(
            r#"
[contracts]
workspace = ["payments", "platform"]
"#,
        );
        let (config, _warnings) = env.load_with_warnings().unwrap();
        assert_eq!(
            config.contracts.workspace,
            vec!["payments".to_string(), "platform".to_string()]
        );
    }

    #[test]
    fn workspace_global_layer_ignored_with_warning() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[contracts]
workspace = "payments"
queue = false
"#,
        );
        env.create_repo();
        let (config, warnings) = env.load_with_warnings().unwrap();
        assert!(
            config.contracts.workspace.is_empty(),
            "global workspace must not apply"
        );
        assert!(
            !config.contracts.queue,
            "other [contracts] keys still layer from global"
        );
        assert_eq!(warnings.len(), 1, "exactly one warning: {warnings:?}");
        assert!(
            warnings[0].contains("workspace is repo-local only"),
            "warning explains the rule: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains(".wonk/config.toml"),
            "warning names where to declare it: {}",
            warnings[0]
        );
    }

    #[test]
    fn workspace_repo_layer_applies() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[contracts]
workspace = "global-ws"
"#,
        );
        env.create_repo();
        env.write_repo_config(
            r#"
[contracts]
workspace = "payments"
"#,
        );
        let (config, _warnings) = env.load_with_warnings().unwrap();
        assert_eq!(
            config.contracts.workspace,
            vec!["payments".to_string()],
            "repo layer wins over the ignored global value"
        );
    }

    #[test]
    fn workspace_absent_defaults_to_empty() {
        let mut env = TestEnv::new();
        env.create_repo();
        env.write_repo_config(
            r#"
[contracts]
queue = false
"#,
        );
        let (config, warnings) = env.load_with_warnings().unwrap();
        assert!(config.contracts.workspace.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn workspace_invalid_type_is_a_parse_error() {
        let mut env = TestEnv::new();
        env.create_repo();
        env.write_repo_config(
            r#"
[contracts]
workspace = 42
"#,
        );
        let result = env.load_with_warnings();
        assert!(result.is_err());
    }

    #[test]
    fn defaults_applied_when_no_config_exists() {
        let env = TestEnv::new();
        // No config files written.
        let config = env.load().unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(
            config.embedding.provider,
            crate::embedding::EmbeddingProviderKind::Bundled
        );
        assert_eq!(config.daemon.debounce_ms, 500);
        assert_eq!(config.index.max_file_size_kb, 1024);
        assert!(config.index.additional_extensions.is_empty());
        assert_eq!(config.output.default_format, "grep");
        assert_eq!(config.output.color, "auto");
        assert!(config.ignore.patterns.is_empty());
    }

    #[test]
    fn reach_defaults_applied_when_no_config_exists() {
        let env = TestEnv::new();
        let config = env.load().unwrap();
        assert_eq!(config.reach.depth, 3);
        assert!(config.reach.enabled);
    }

    #[test]
    fn contracts_defaults_applied_when_no_config_exists() {
        let env = TestEnv::new();
        let config = env.load().unwrap();
        assert!(config.contracts.http);
        assert!(config.contracts.env);
        assert!(config.contracts.queue);
        assert!(config.contracts.websocket);
        assert!(config.contracts.job);
        assert!(config.contracts.grpc);
        assert!(config.contracts.graphql);
        assert!(config.contracts.openapi);
    }

    #[test]
    fn contracts_reads_from_config_file() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[contracts]
queue = false
websocket = false
"#,
        );
        let config = env.load().unwrap();
        assert!(!config.contracts.queue);
        assert!(!config.contracts.websocket);
        assert!(config.contracts.http);
    }

    #[test]
    fn contracts_rpc_kinds_read_from_config_file() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[contracts]
grpc = false
graphql = false
openapi = false
"#,
        );
        let config = env.load().unwrap();
        assert!(!config.contracts.grpc);
        assert!(!config.contracts.graphql);
        assert!(!config.contracts.openapi);
    }

    #[test]
    fn contracts_rpc_kinds_partial_overlay_keeps_unset_defaults() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[contracts]
grpc = false
"#,
        );
        let config = env.load().unwrap();
        assert!(!config.contracts.grpc);
        assert!(config.contracts.graphql);
        assert!(config.contracts.openapi);
    }

    #[test]
    fn contracts_partial_overlays_only_override_set_fields() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[contracts]
queue = false
"#,
        );
        let config = env.load().unwrap();
        assert!(!config.contracts.queue);
        assert!(
            config.contracts.env,
            "absent env key keeps the default (all kinds on)"
        );
        assert!(config.contracts.http);
        assert!(config.contracts.websocket);
        assert!(config.contracts.job);
    }

    #[test]
    fn contracts_repo_layer_overrides_global() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[contracts]
queue = false
job = false
"#,
        );
        env.create_repo();
        env.write_repo_config(
            r#"
[contracts]
queue = true
"#,
        );
        let config = env.load().unwrap();
        assert!(config.contracts.queue, "repo layer wins");
        assert!(!config.contracts.job, "global job survives repo layer");
    }

    #[test]
    fn reach_reads_from_config_file() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[reach]
depth = 2
enabled = false
"#,
        );
        let config = env.load().unwrap();
        assert_eq!(config.reach.depth, 2);
        assert!(!config.reach.enabled);
    }

    #[test]
    fn reach_partial_overlays_only_override_set_fields() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[reach]
depth = 1
"#,
        );
        let config = env.load().unwrap();
        assert_eq!(config.reach.depth, 1);
        assert!(config.reach.enabled, "absent enabled key keeps the default");
    }

    #[test]
    fn reach_repo_layer_overrides_global() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[reach]
depth = 5
enabled = true
"#,
        );
        env.create_repo();
        env.write_repo_config(
            r#"
[reach]
enabled = false
"#,
        );
        let config = env.load().unwrap();
        assert_eq!(config.reach.depth, 5, "global depth survives repo layer");
        assert!(!config.reach.enabled, "repo layer wins");
    }

    #[test]
    fn review_defaults_applied_when_no_config_exists() {
        let env = TestEnv::new();
        let config = env.load().unwrap();
        assert!(config.review.breaking_change);
        assert!(config.review.coverage_gap);
        assert!(config.review.cross_repo);
    }

    #[test]
    fn review_reads_from_config_file() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[review]
breaking_change = false
coverage_gap = false
cross_repo = false
"#,
        );
        let config = env.load().unwrap();
        assert!(!config.review.breaking_change);
        assert!(!config.review.coverage_gap);
        assert!(!config.review.cross_repo);
    }

    #[test]
    fn review_partial_overlays_only_override_set_fields() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[review]
coverage_gap = false
"#,
        );
        let config = env.load().unwrap();
        assert!(
            config.review.breaking_change,
            "absent breaking_change key keeps the default"
        );
        assert!(!config.review.coverage_gap);
        assert!(
            config.review.cross_repo,
            "absent cross_repo key keeps the default"
        );
    }

    #[test]
    fn review_rules_layer_independently_across_layers() {
        // A noisy rule can be silenced alone, in any layer, without touching
        // the other family (AR-022).
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[review]
coverage_gap = false
"#,
        );
        env.create_repo();
        env.write_repo_config(
            r#"
[review]
breaking_change = false
cross_repo = false
"#,
        );
        let config = env.load().unwrap();
        assert!(
            !config.review.coverage_gap,
            "global silencing survives the repo layer"
        );
        assert!(
            !config.review.breaking_change,
            "repo layer silences the other family alone"
        );
        assert!(
            !config.review.cross_repo,
            "repo layer silences the third family alone"
        );
    }

    #[test]
    fn embedding_provider_follows_global_then_repo_precedence() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[embedding]
provider = "ollama"
"#,
        );
        let repo = env.create_repo();
        env.write_repo_config(
            r#"
[embedding]
provider = "bundled"
"#,
        );

        let global =
            Config::load_with_global_dir(Some(&env.global_path), None).expect("global config");
        assert_eq!(
            global.embedding.provider,
            crate::embedding::EmbeddingProviderKind::Ollama
        );

        let resolved =
            Config::load_with_global_dir(Some(&env.global_path), Some(&repo)).expect("repo config");
        assert_eq!(
            resolved.embedding.provider,
            crate::embedding::EmbeddingProviderKind::Bundled
        );
    }

    #[test]
    fn invalid_embedding_provider_is_rejected() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[embedding]
provider = "remote"
"#,
        );

        let error = env.load().unwrap_err().to_string();
        assert!(error.contains("failed to parse config file"));
    }

    #[test]
    fn global_config_overrides_defaults() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[daemon]
debounce_ms = 200

[output]
default_format = "json"
"#,
        );

        let config = env.load().unwrap();
        // Overridden values:
        assert_eq!(config.daemon.debounce_ms, 200);
        assert_eq!(config.output.default_format, "json");
        // Default values should remain:
        assert_eq!(config.index.max_file_size_kb, 1024);
        assert_eq!(config.output.color, "auto");
    }

    #[test]
    fn repo_config_overrides_global() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[daemon]
debounce_ms = 200

[output]
color = "always"
"#,
        );

        let repo = env.create_repo();
        env.write_repo_config(
            r#"
[daemon]
debounce_ms = 100

[index]
max_file_size_kb = 512
additional_extensions = ["toml", "yaml"]
"#,
        );

        let config = Config::load_with_global_dir(Some(&env.global_path), Some(&repo)).unwrap();
        // Per-repo overrides global:
        assert_eq!(config.daemon.debounce_ms, 100);
        // Per-repo sets index fields:
        assert_eq!(config.index.max_file_size_kb, 512);
        assert_eq!(
            config.index.additional_extensions,
            vec!["toml".to_string(), "yaml".to_string()]
        );
        // Global value not overridden by repo should still be present:
        assert_eq!(config.output.color, "always");
        // Default not touched by either layer:
        assert_eq!(config.output.default_format, "grep");
    }

    #[test]
    fn partial_sections_only_override_specified_fields() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[daemon]
debounce_ms = 100
"#,
        );

        let config = env.load().unwrap();
        // Only debounce_ms was set; other defaults should remain.
        assert_eq!(config.daemon.debounce_ms, 100);
        assert_eq!(config.index.max_file_size_kb, 1024);
    }

    #[test]
    fn ignore_patterns_from_config() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[ignore]
patterns = ["*.log", "tmp/"]
"#,
        );

        let repo = env.create_repo();
        env.write_repo_config(
            r#"
[ignore]
patterns = ["*.bak"]
"#,
        );

        let config = Config::load_with_global_dir(Some(&env.global_path), Some(&repo)).unwrap();
        // Per-repo replaces the global patterns (last wins for the whole list).
        assert_eq!(config.ignore.patterns, vec!["*.bak".to_string()]);
    }

    #[test]
    fn invalid_toml_produces_clear_error() {
        let env = TestEnv::new();
        env.write_global_config("this is [[[not valid toml");

        let result = env.load();
        assert!(result.is_err());
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("failed to parse config file"),
            "error should mention parsing failure, got: {err_msg}"
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // If the config file has keys we don't recognize (e.g., from a
        // future version), we should not error out.
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[daemon]
debounce_ms = 250
some_future_key = true

[some_future_section]
value = 42
"#,
        );

        let config = env.load().unwrap();
        assert_eq!(config.daemon.debounce_ms, 250);
    }

    #[test]
    fn wrong_type_produces_error() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[daemon]
debounce_ms = "not a number"
"#,
        );

        let result = env.load();
        assert!(result.is_err());
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("failed to parse config file"),
            "error should mention parsing failure, got: {err_msg}"
        );
    }

    #[test]
    fn empty_config_files_are_fine() {
        let mut env = TestEnv::new();
        env.write_global_config("");
        let repo = env.create_repo();
        env.write_repo_config("");

        let config = Config::load_with_global_dir(Some(&env.global_path), Some(&repo)).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn repo_config_without_global() {
        let mut env = TestEnv::new();
        // No global config written -- global dir exists but has no config.toml.
        let repo = env.create_repo();
        env.write_repo_config(
            r#"
[output]
default_format = "json"
color = "never"
"#,
        );

        let config = Config::load_with_global_dir(Some(&env.global_path), Some(&repo)).unwrap();
        assert_eq!(config.output.default_format, "json");
        assert_eq!(config.output.color, "never");
        // Everything else should be defaults.
        assert_eq!(config.daemon.debounce_ms, 500);
    }

    #[test]
    fn all_config_fields_round_trip() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[daemon]
debounce_ms = 250

[index]
max_file_size_kb = 2048
additional_extensions = ["md", "txt"]

[output]
default_format = "json"
color = "never"

[ignore]
patterns = ["*.tmp", "cache/"]
"#,
        );

        let config = env.load().unwrap();
        assert_eq!(config.daemon.debounce_ms, 250);
        assert_eq!(config.index.max_file_size_kb, 2048);
        assert_eq!(
            config.index.additional_extensions,
            vec!["md".to_string(), "txt".to_string()]
        );
        assert_eq!(config.output.default_format, "json");
        assert_eq!(config.output.color, "never");
        assert_eq!(
            config.ignore.patterns,
            vec!["*.tmp".to_string(), "cache/".to_string()]
        );
    }

    #[test]
    fn legacy_idle_timeout_minutes_silently_ignored() {
        // Old config files may still contain idle_timeout_minutes.
        // Since DaemonOverlay uses #[serde(default)] without deny_unknown_fields,
        // the key should be silently ignored with no error.
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[daemon]
idle_timeout_minutes = 60
debounce_ms = 200
"#,
        );

        let config = env.load().unwrap();
        // idle_timeout_minutes is ignored; debounce_ms is applied.
        assert_eq!(config.daemon.debounce_ms, 200);
    }

    #[test]
    fn no_global_dir_uses_only_defaults() {
        // When there is no home directory (and no repo), we get pure defaults.
        let config = Config::load_with_global_dir(None, None).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn three_layer_merge_works() {
        // Verify full 3-layer merge: default -> global -> repo
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[daemon]
debounce_ms = 200

[index]
max_file_size_kb = 2048

[output]
default_format = "json"
color = "always"

[ignore]
patterns = ["*.log"]
"#,
        );

        let repo = env.create_repo();
        env.write_repo_config(
            r#"
[daemon]
debounce_ms = 100

[output]
color = "never"
"#,
        );

        let config = Config::load_with_global_dir(Some(&env.global_path), Some(&repo)).unwrap();

        // From global:
        assert_eq!(config.index.max_file_size_kb, 2048);
        assert_eq!(config.output.default_format, "json");
        assert_eq!(config.ignore.patterns, vec!["*.log".to_string()]);

        // Overridden by repo:
        assert_eq!(config.daemon.debounce_ms, 100);
        assert_eq!(config.output.color, "never");

        // Still at default (not set in either config):
        assert!(config.index.additional_extensions.is_empty());
    }

    // -- LLM config tests ---------------------------------------------------

    #[test]
    fn llm_defaults_applied_when_no_config() {
        let env = TestEnv::new();
        let config = env.load().unwrap();
        assert_eq!(config.llm.model, "llama3.2:3b");
        assert_eq!(
            config.llm.generate_url,
            "http://localhost:11434/api/generate"
        );
    }

    #[test]
    fn llm_model_override_from_global() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[llm]
model = "mistral:7b"
"#,
        );

        let config = env.load().unwrap();
        assert_eq!(config.llm.model, "mistral:7b");
        // generate_url should remain default.
        assert_eq!(
            config.llm.generate_url,
            "http://localhost:11434/api/generate"
        );
    }

    #[test]
    fn llm_generate_url_override() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[llm]
generate_url = "http://myhost:8080/api/generate"
"#,
        );

        let config = env.load().unwrap();
        assert_eq!(config.llm.generate_url, "http://myhost:8080/api/generate");
        // model should remain default.
        assert_eq!(config.llm.model, "llama3.2:3b");
    }

    #[test]
    fn llm_repo_overrides_global() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[llm]
model = "mistral:7b"
generate_url = "http://global:11434/api/generate"
"#,
        );

        let repo = env.create_repo();
        env.write_repo_config(
            r#"
[llm]
model = "llama3.2:1b"
"#,
        );

        let config = Config::load_with_global_dir(Some(&env.global_path), Some(&repo)).unwrap();
        assert_eq!(config.llm.model, "llama3.2:1b");
        // generate_url from global should survive.
        assert_eq!(config.llm.generate_url, "http://global:11434/api/generate");
    }

    // -- Search config tests --------------------------------------------------

    #[test]
    fn search_rrf_k_default_is_60() {
        let env = TestEnv::new();
        let config = env.load().unwrap();
        assert!((config.search.rrf_k - 60.0).abs() < f32::EPSILON);
    }

    #[test]
    fn search_bm25_defaults() {
        let env = TestEnv::new();
        let config = env.load().unwrap();
        assert!((config.search.bm25_k1 - 1.2).abs() < f32::EPSILON);
        assert!((config.search.bm25_b - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn search_bm25_override_from_global() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[search]
bm25_k1 = 2.0
bm25_b = 0.5
"#,
        );

        let config = env.load().unwrap();
        assert!((config.search.bm25_k1 - 2.0).abs() < f32::EPSILON);
        assert!((config.search.bm25_b - 0.5).abs() < f32::EPSILON);
        // rrf_k should remain default.
        assert!((config.search.rrf_k - 60.0).abs() < f32::EPSILON);
    }

    #[test]
    fn search_bm25_repo_overrides_global() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[search]
bm25_k1 = 2.0
bm25_b = 0.5
"#,
        );

        let repo = env.create_repo();
        env.write_repo_config(
            r#"
[search]
bm25_b = 0.3
"#,
        );

        let config = Config::load_with_global_dir(Some(&env.global_path), Some(&repo)).unwrap();
        // bm25_b from repo wins; bm25_k1 from global survives.
        assert!((config.search.bm25_k1 - 2.0).abs() < f32::EPSILON);
        assert!((config.search.bm25_b - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn search_rrf_k_override_from_global() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[search]
rrf_k = 40.0
"#,
        );

        let config = env.load().unwrap();
        assert!((config.search.rrf_k - 40.0).abs() < f32::EPSILON);
    }

    #[test]
    fn search_rrf_k_repo_overrides_global() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[search]
rrf_k = 40.0
"#,
        );

        let repo = env.create_repo();
        env.write_repo_config(
            r#"
[search]
rrf_k = 80.0
"#,
        );

        let config = Config::load_with_global_dir(Some(&env.global_path), Some(&repo)).unwrap();
        assert!((config.search.rrf_k - 80.0).abs() < f32::EPSILON);
    }

    // -- Rank config tests --------------------------------------------------

    #[test]
    fn rank_defaults_to_disabled_with_kind_weight() {
        // REQ-017: reranking is behind config defaulting to the current
        // ordering. The default flip is TASK-095's, not ours.
        let env = TestEnv::new();
        let config = env.load().unwrap();
        assert!(!config.rank.enabled);
        assert_eq!(
            config.rank.weights,
            HashMap::from([("kind".to_string(), 1.0)])
        );
    }

    #[test]
    fn rank_reads_from_config_file() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[rank]
enabled = true

[rank.weights]
kind = 2.0
"#,
        );
        let config = env.load().unwrap();
        assert!(config.rank.enabled);
        assert_eq!(
            config.rank.weights,
            HashMap::from([("kind".to_string(), 2.0)])
        );
    }

    #[test]
    fn rank_enabled_and_weights_layer_independently() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[rank]
enabled = true
"#,
        );
        env.create_repo();
        env.write_repo_config(
            r#"
[rank.weights]
kind = 0.5
"#,
        );
        let config = env.load_with_warnings().unwrap().0;
        // Global enabled survives a repo layer that only sets weights.
        assert!(config.rank.enabled);
        assert_eq!(
            config.rank.weights,
            HashMap::from([("kind".to_string(), 0.5)])
        );
    }

    #[test]
    fn rank_weights_replace_wholesale_per_layer() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[rank.weights]
kind = 2.0
"#,
        );
        env.create_repo();
        env.write_repo_config(
            r#"
[rank.weights]
kind = 0.0
"#,
        );
        let config = env.load().unwrap();
        // The repo table replaces the global one (last wins for the whole
        // table), it does not merge per-key with defaults or prior layers.
        assert_eq!(
            config.rank.weights,
            HashMap::from([("kind".to_string(), 0.0)])
        );
    }

    #[test]
    fn rank_unknown_signal_name_is_a_hard_error() {
        // REQ-006: unknown names are rejected, not silently ignored.
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[rank.weights]
nosuch_signal = 1.0
"#,
        );
        let err = env.load().unwrap_err().to_string();
        assert!(
            err.contains("unknown signal name 'nosuch_signal' in [rank.weights]"),
            "error names the offender and the section: {err}"
        );
        assert!(
            err.contains("known: kind"),
            "error lists valid names: {err}"
        );
    }

    #[test]
    fn rank_unknown_signal_name_in_repo_layer_is_a_hard_error() {
        let mut env = TestEnv::new();
        env.create_repo();
        env.write_repo_config(
            r#"
[rank.weights]
nosuch_signal = 3.0
"#,
        );
        assert!(env.load().is_err());
    }

    #[test]
    fn rank_non_finite_weight_is_a_hard_error() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[rank.weights]
kind = nan
"#,
        );
        let err = env.load().unwrap_err().to_string();
        assert!(err.contains("non-finite weight"), "got: {err}");
    }

    #[test]
    fn rank_section_absent_keeps_defaults() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[search]
rrf_k = 40.0
"#,
        );
        let config = env.load().unwrap();
        assert!(!config.rank.enabled);
        assert_eq!(
            config.rank.weights,
            HashMap::from([("kind".to_string(), 1.0)])
        );
    }

    // -------------------------------------------------------------------
    // TASK-095: [rank.class_multipliers]
    // -------------------------------------------------------------------

    #[test]
    fn rank_class_multipliers_default_neutral() {
        let env = TestEnv::new();
        let config = env.load().unwrap();
        assert_eq!(
            config.rank.class_multipliers,
            crate::rerank::ClassMultipliers::neutral()
        );
    }

    #[test]
    fn rank_class_multipliers_read_from_config_file() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[rank]
enabled = true

[rank.class_multipliers.symbol]
lexical = 1.5
semantic = 0.5

[rank.class_multipliers.path]
lexical = 0.8

[rank.class_multipliers.signature]
semantic = 0.9
"#,
        );
        let config = env.load().unwrap();
        assert!(config.rank.enabled);
        let m = &config.rank.class_multipliers;
        assert_eq!(
            m.symbol,
            crate::rerank::ChannelMultipliers {
                lexical: 1.5,
                semantic: 0.5
            }
        );
        // A channel absent from the table defaults to neutral 1.0.
        assert_eq!(m.path.lexical, 0.8);
        assert_eq!(m.path.semantic, 1.0);
        assert_eq!(m.signature.lexical, 1.0);
        assert_eq!(m.signature.semantic, 0.9);
    }

    #[test]
    fn rank_class_multipliers_replace_wholesale_per_layer() {
        let mut env = TestEnv::new();
        env.write_global_config(
            r#"
[rank.class_multipliers.symbol]
lexical = 2.0
"#,
        );
        env.create_repo();
        env.write_repo_config(
            r#"
[rank.class_multipliers.symbol]
lexical = 0.5
"#,
        );
        let config = env.load().unwrap();
        // The repo table replaces the global one for the classes it names;
        // classes it omits fall back to the DEFAULT (neutral), not the
        // global layer — same wholesale semantics as [rank.weights].
        assert_eq!(config.rank.class_multipliers.symbol.lexical, 0.5);
        assert_eq!(config.rank.class_multipliers.path.lexical, 1.0);
    }

    #[test]
    fn rank_class_multipliers_unknown_class_is_a_hard_error() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[rank.class_multipliers.troll]
lexical = 1.0
"#,
        );
        let err = env.load().unwrap_err().to_string();
        assert!(
            err.contains("unknown query class 'troll' in [rank.class_multipliers]"),
            "error names the offender and the section: {err}"
        );
        assert!(
            err.contains("known: symbol, path, signature"),
            "error lists the valid classes: {err}"
        );
    }

    #[test]
    fn rank_class_multipliers_conceptual_is_rejected_citing_req009() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[rank.class_multipliers.conceptual]
lexical = 1.0
"#,
        );
        let err = env.load().unwrap_err().to_string();
        assert!(
            err.contains("[rank.class_multipliers.conceptual]"),
            "error names the rejected table: {err}"
        );
        assert!(
            err.contains("REQ-009"),
            "error cites the requirement pinning neutrality: {err}"
        );
    }

    #[test]
    fn rank_class_multipliers_negative_is_a_hard_error() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[rank.class_multipliers.symbol]
lexical = -0.5
"#,
        );
        let err = env.load().unwrap_err().to_string();
        assert!(
            err.contains("class multiplier"),
            "error names the field: {err}"
        );
        assert!(err.contains("-0.5"), "error names the value: {err}");
    }

    #[test]
    fn rank_class_multipliers_non_finite_is_a_hard_error() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[rank.class_multipliers.path]
semantic = nan
"#,
        );
        let err = env.load().unwrap_err().to_string();
        assert!(
            err.contains("class multiplier"),
            "error names the field: {err}"
        );
    }
}
