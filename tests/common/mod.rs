//! Fixture commands inherit only explicit fixture configuration.
#![allow(dead_code)]
use std::path::Path;
use std::process::Command;

/// HOME lives inside the lifetime-owned repository tempdir and is hidden from
/// the walker. It survives spawned MCP children until the fixture is dropped.
pub fn isolate_command(command: &mut Command, root: &Path) {
    let home = root.join(".wonk-test-home");
    std::fs::create_dir_all(&home).unwrap();
    command.env("HOME", home);
}

pub fn command(binary: impl AsRef<std::ffi::OsStr>, root: &Path) -> Command {
    let mut command = Command::new(binary);
    command.current_dir(root);
    isolate_command(&mut command, root);
    command
}

/// Build only from defaults and explicit repository fixture configuration.
pub fn build_index(root: &Path, local: bool) -> anyhow::Result<wonk::pipeline::IndexStats> {
    let root = root.canonicalize()?;
    let config = wonk::config::Config::load_with_paths(None, Some(&root))?;
    wonk::pipeline::build_index_with_config(&root, local, &config)
}

/// Update only from defaults and the explicit fixture repository layer.
pub fn incremental_update(root: &Path, local: bool) -> anyhow::Result<wonk::pipeline::IndexStats> {
    let root = root.canonicalize()?;
    let config = wonk::config::Config::load_with_paths(None, Some(&root))?;
    wonk::pipeline::incremental_update_with_config(&root, local, &config)
}
