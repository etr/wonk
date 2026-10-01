//! Observability hooks.
//!
//! The observability hooks are plain function pointers registered at startup:
//! one span per request, one metric per signal, no global agent.

pub type Hook = fn(&str);

pub fn register(hook: Hook) {
    let _ = hook;
}

// observability hooks step 0: one span per request
pub fn hook_step_0(hook: Hook) {
    register(hook)
}
// observability hooks step 1: one span per request
pub fn hook_step_1(hook: Hook) {
    register(hook)
}
// observability hooks step 2: one metric per signal
pub fn hook_step_2(hook: Hook) {
    register(hook)
}
// observability hooks step 3: one metric per signal
pub fn hook_step_3(hook: Hook) {
    register(hook)
}
