//! Configuration file parsing, defaults, and merging.
//!
//! Configuration is loaded in layers (last wins):
//! 1. Built-in defaults
//! 2. Global config from `~/.wonk/config.toml`
//! 3. Per-repo config from `<repo_root>/.wonk/config.toml`
//!
//! Each layer only overrides fields it explicitly sets; absent fields
//! are left at their previous value.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

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
}

/// Daemon-related settings.
#[derive(Debug, Clone, PartialEq)]
pub struct DaemonConfig {
    /// How many minutes of inactivity before the daemon shuts down.
    pub idle_timeout_minutes: u64,
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

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout_minutes: 30,
            debounce_ms: 500,
        }
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
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct DaemonOverlay {
    idle_timeout_minutes: Option<u64>,
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

// ---------------------------------------------------------------------------
// Merge helpers
// ---------------------------------------------------------------------------

impl Config {
    /// Apply an overlay on top of this config, replacing only the fields
    /// that are `Some` in the overlay.
    fn apply_overlay(&mut self, overlay: ConfigOverlay) {
        if let Some(d) = overlay.daemon {
            if let Some(v) = d.idle_timeout_minutes {
                self.daemon.idle_timeout_minutes = v;
            }
            if let Some(v) = d.debounce_ms {
                self.daemon.debounce_ms = v;
            }
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
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Return the user's home directory.
fn home_dir() -> Option<PathBuf> {
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
        Self::load_with_global_dir(global_dir.as_deref(), repo_root)
    }

    /// Internal: load config with an explicit global config directory.
    ///
    /// This allows tests to supply a temporary directory instead of the
    /// real `~/.wonk` without mutating environment variables.
    fn load_with_global_dir(global_dir: Option<&Path>, repo_root: Option<&Path>) -> Result<Config> {
        let mut config = Config::default();

        // Layer 2: global config
        if let Some(dir) = global_dir {
            let global_path = dir.join("config.toml");
            if let Some(overlay) = load_overlay(&global_path)? {
                config.apply_overlay(overlay);
            }
        }

        // Layer 3: per-repo config
        if let Some(root) = repo_root {
            let repo_config_path = root.join(".wonk").join("config.toml");
            if let Some(overlay) = load_overlay(&repo_config_path)? {
                config.apply_overlay(overlay);
            }
        }

        Ok(config)
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
    }

    #[test]
    fn defaults_applied_when_no_config_exists() {
        let env = TestEnv::new();
        // No config files written.
        let config = env.load().unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.daemon.idle_timeout_minutes, 30);
        assert_eq!(config.daemon.debounce_ms, 500);
        assert_eq!(config.index.max_file_size_kb, 1024);
        assert!(config.index.additional_extensions.is_empty());
        assert_eq!(config.output.default_format, "grep");
        assert_eq!(config.output.color, "auto");
        assert!(config.ignore.patterns.is_empty());
    }

    #[test]
    fn global_config_overrides_defaults() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[daemon]
idle_timeout_minutes = 60
debounce_ms = 200

[output]
default_format = "json"
"#,
        );

        let config = env.load().unwrap();
        // Overridden values:
        assert_eq!(config.daemon.idle_timeout_minutes, 60);
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
idle_timeout_minutes = 60

[output]
color = "always"
"#,
        );

        let repo = env.create_repo();
        env.write_repo_config(
            r#"
[daemon]
idle_timeout_minutes = 10

[index]
max_file_size_kb = 512
additional_extensions = ["toml", "yaml"]
"#,
        );

        let config = Config::load_with_global_dir(Some(&env.global_path), Some(&repo)).unwrap();
        // Per-repo overrides global:
        assert_eq!(config.daemon.idle_timeout_minutes, 10);
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
        assert_eq!(config.daemon.debounce_ms, 500);
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
        // Only debounce_ms was set; idle_timeout_minutes should be the default.
        assert_eq!(config.daemon.debounce_ms, 100);
        assert_eq!(config.daemon.idle_timeout_minutes, 30);
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
idle_timeout_minutes = 45
some_future_key = true

[some_future_section]
value = 42
"#,
        );

        let config = env.load().unwrap();
        assert_eq!(config.daemon.idle_timeout_minutes, 45);
    }

    #[test]
    fn wrong_type_produces_error() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[daemon]
idle_timeout_minutes = "not a number"
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
        assert_eq!(config.daemon.idle_timeout_minutes, 30);
    }

    #[test]
    fn all_config_fields_round_trip() {
        let env = TestEnv::new();
        env.write_global_config(
            r#"
[daemon]
idle_timeout_minutes = 15
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
        assert_eq!(config.daemon.idle_timeout_minutes, 15);
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
idle_timeout_minutes = 60
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
        assert_eq!(config.daemon.idle_timeout_minutes, 60);
        assert_eq!(config.index.max_file_size_kb, 2048);
        assert_eq!(config.output.default_format, "json");
        assert_eq!(config.ignore.patterns, vec!["*.log".to_string()]);

        // Overridden by repo:
        assert_eq!(config.daemon.debounce_ms, 100);
        assert_eq!(config.output.color, "never");

        // Still at default (not set in either config):
        assert!(config.index.additional_extensions.is_empty());
    }
}
