//! Observability hooks.
//!
//! The observability hooks are plain function pointers registered at startup:
//! one span per request, one metric per signal, no global agent.

pub type Hook = fn(&str);

pub fn register(hook: Hook) {
    let _ = hook;
}
