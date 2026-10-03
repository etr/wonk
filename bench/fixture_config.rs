//! Deterministic fixture setup for optimized benchmarks.
pub fn build_index(
    root: &std::path::Path,
    local: bool,
) -> anyhow::Result<wonk::pipeline::IndexStats> {
    let root = root.canonicalize()?;
    let config = wonk::config::Config::load_with_paths(None, Some(&root))?;
    wonk::pipeline::build_index_with_config(&root, local, &config)
}
