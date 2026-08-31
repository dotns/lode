//! Tracing subscriber setup, extracted from `main` so every entry point shares
//! one initialisation path: an `EnvFilter` at the requested level (falling back
//! to `info` on a bad filter string), with target names suppressed.

/// Initialise the global tracing subscriber at `level`, falling back to `info`
/// if the filter string is invalid. Idempotent: a second call is a no-op.
pub(crate) fn init(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
