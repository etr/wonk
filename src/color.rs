//! Color resolution: determines whether to emit ANSI color codes.
//!
//! Priority chain (highest first):
//! 1. `NO_COLOR` env (any value) → false
//! 2. `CLICOLOR_FORCE=1` env → true
//! 3. Config `"always"` or `"true"` → true
//! 4. Config `"never"` or `"false"` → false
//! 5. `CLICOLOR=0` env → false
//! 6. TTY detection on stdout → true if terminal, false otherwise

// ---------------------------------------------------------------------------
// ANSI escape constants (matching ripgrep conventions)
// ---------------------------------------------------------------------------
//
// Accessibility note (deuteranopia / protanopia):
// Red (MATCH) and green (LINE_NO) appear on structurally distinct elements —
// line numbers vs inline content highlights — so they are never used to
// distinguish between two states of the same element. Additionally, MATCH
// uses bold + underline as non-color indicators, ensuring that match
// highlights remain visually distinct even without color perception.

/// Reset all attributes.
pub const RESET: &str = "\x1b[0m";
/// File paths: magenta + bold.
pub const FILE: &str = "\x1b[35m\x1b[1m";
/// Line numbers: green.
pub const LINE_NO: &str = "\x1b[32m";
/// Match highlights: red + bold + underline.
/// Bold and underline serve as non-color indicators for accessibility.
pub const MATCH: &str = "\x1b[1m\x1b[4m\x1b[31m";
/// Separators (colons): cyan.
pub const SEP: &str = "\x1b[36m";

// ---------------------------------------------------------------------------
// Color resolution
// ---------------------------------------------------------------------------

/// Resolve whether to use color based on environment variables, config, and TTY.
pub fn resolve_color(config_color: &str) -> bool {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let clicolor_force = std::env::var("CLICOLOR_FORCE").ok();
    let clicolor = std::env::var("CLICOLOR").ok();
    let is_tty = {
        use std::io::IsTerminal;
        std::io::stdout().is_terminal()
    };
    resolve_color_inner(
        no_color,
        clicolor_force.as_deref(),
        config_color,
        clicolor.as_deref(),
        is_tty,
    )
}

/// Inner resolution logic, fully parameterized for testability.
pub fn resolve_color_inner(
    no_color: bool,
    clicolor_force: Option<&str>,
    config_color: &str,
    clicolor: Option<&str>,
    is_tty: bool,
) -> bool {
    // 1. NO_COLOR (any value) disables color
    if no_color {
        return false;
    }
    // 2. CLICOLOR_FORCE=1 forces color
    if clicolor_force == Some("1") {
        return true;
    }
    // 3. Config "always" or "true" enables color
    match config_color {
        "always" | "true" => return true,
        "never" | "false" => return false,
        _ => {} // "auto" or unrecognized → continue
    }
    // 5. CLICOLOR=0 disables color
    if clicolor == Some("0") {
        return false;
    }
    // 6. TTY detection
    is_tty
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_env_disables_color() {
        assert!(!resolve_color_inner(true, None, "auto", None, true));
    }

    #[test]
    fn no_color_takes_precedence_over_clicolor_force() {
        assert!(!resolve_color_inner(true, Some("1"), "auto", None, true));
    }

    #[test]
    fn clicolor_force_enables_color() {
        assert!(resolve_color_inner(false, Some("1"), "auto", None, false));
    }

    #[test]
    fn config_always_enables_color() {
        assert!(resolve_color_inner(false, None, "always", None, false));
    }

    #[test]
    fn config_true_enables_color() {
        assert!(resolve_color_inner(false, None, "true", None, false));
    }

    #[test]
    fn config_never_disables_color() {
        assert!(!resolve_color_inner(false, None, "never", None, true));
    }

    #[test]
    fn config_false_disables_color() {
        assert!(!resolve_color_inner(false, None, "false", None, true));
    }

    #[test]
    fn clicolor_zero_disables_color() {
        assert!(!resolve_color_inner(false, None, "auto", Some("0"), true));
    }

    #[test]
    fn tty_true_enables_color_in_auto_mode() {
        assert!(resolve_color_inner(false, None, "auto", None, true));
    }

    #[test]
    fn tty_false_disables_color_in_auto_mode() {
        assert!(!resolve_color_inner(false, None, "auto", None, false));
    }

    #[test]
    fn clicolor_force_overrides_config_never() {
        // CLICOLOR_FORCE=1 has higher priority than config "never"
        assert!(resolve_color_inner(false, Some("1"), "never", None, false));
    }

    #[test]
    fn config_always_overrides_clicolor_zero() {
        // Config "always" has higher priority than CLICOLOR=0
        assert!(resolve_color_inner(false, None, "always", Some("0"), false));
    }
}
