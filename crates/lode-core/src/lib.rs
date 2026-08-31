#![forbid(unsafe_code)]
//! lode-core — the clap-free, signal-free core of lode.
//!
//! Everything lode does *besides* owning a process: configuration, manifest
//! resolution, verified download + install, activation, and the operator
//! commands, exposed through the [`Engine`] facade. It knows nothing about
//! argument parsing (that is the `lode` binary) or signal handling and the
//! supervise loop (that is `lode-supervisor`), so a host application can embed
//! it without inheriting either.
//!
//! Verification is two-layer: integrity (sha256) + publisher identity (ed25519).
//! See `docs/architecture.md`.
//!
//! ```no_run
//! # fn main() -> lode_core::Result<()> {
//! let cfg = lode_core::Config::from_toml("")?;
//! let engine = lode_core::Engine::new(cfg);
//! let latest = engine.check()?;
//! println!("{} {} -> {}", latest.app, latest.channel, latest.version);
//! # Ok(())
//! # }
//! ```

pub mod commands;
pub mod config;
pub mod download;
pub mod engine;
mod error;
pub mod http;
mod idval;
pub mod install;
pub mod lock;
mod logging;
pub mod manifest;
pub mod state;
pub mod verify;

pub use crate::config::{
    Command, Config, ConfigBuilder, Global, Http, Overrides, Policy, Readiness, RequireSignature,
    RestartMode, RestartPolicy, Runtime, Signals, Supervise, Trust, Update,
};
pub use crate::engine::{CheckResult, Engine};
pub use crate::error::{Error, Result};

/// Opt-in installer for lode's process-global side effects.
///
/// The `lode`/`lode-cli` binary installs all four — the rustls crypto provider,
/// core-dump suppression, the global tracing subscriber, and the panic hook. A
/// library consumer embedding the core/supervisor gets **none** of them
/// implicitly and selects only what it wants — e.g. the crypto provider without
/// taking over the host's global tracing subscriber or panic hook.
///
/// [`install`](Self::install) applies the selected effects in a fixed order:
/// crypto provider, core-dump suppression, logging, then panic hook. Every step
/// is individually idempotent/best-effort, so calling it more than once (as the
/// binary does, split across its pre- and post-parse phases) is safe.
///
/// ```no_run
/// // Library consumer: crypto provider only, leave logging to the host.
/// lode_core::InitOptions::new().crypto_provider(true).install();
/// ```
#[derive(Debug, Default, Clone)]
pub struct InitOptions {
    crypto_provider: bool,
    suppress_core_dumps: bool,
    panic_hook: bool,
    logging: Option<String>,
}

impl InitOptions {
    /// An empty set — nothing is installed until an effect is opted in.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// All four effects, with the tracing subscriber at `level` (e.g. "info") —
    /// what the `lode` binary installs, expressed in one call.
    #[must_use]
    pub fn all(level: impl Into<String>) -> Self {
        Self {
            crypto_provider: true,
            suppress_core_dumps: true,
            panic_hook: true,
            logging: Some(level.into()),
        }
    }

    /// Select the process-wide rustls crypto provider (aws-lc-rs, pma-rust
    /// Lock 2). Installing it is idempotent.
    #[must_use]
    pub const fn crypto_provider(mut self, on: bool) -> Self {
        self.crypto_provider = on;
        self
    }

    /// Select best-effort core-dump suppression (rlimit `CORE` = 0).
    #[must_use]
    pub const fn suppress_core_dumps(mut self, on: bool) -> Self {
        self.suppress_core_dumps = on;
        self
    }

    /// Select the tracing-based panic hook.
    #[must_use]
    pub const fn panic_hook(mut self, on: bool) -> Self {
        self.panic_hook = on;
        self
    }

    /// Select the global tracing subscriber: `Some(level)` installs it at that
    /// filter level, `None` leaves the host's subscriber untouched.
    #[must_use]
    pub fn logging(mut self, level: Option<&str>) -> Self {
        self.logging = level.map(ToOwned::to_owned);
        self
    }

    /// Select the global tracing subscriber at `level` — convenience for
    /// [`logging`](Self::logging) with `Some`.
    #[must_use]
    pub fn log_level(mut self, level: impl Into<String>) -> Self {
        self.logging = Some(level.into());
        self
    }

    /// Install only the selected effects, in a fixed order: crypto provider,
    /// core-dump suppression, logging, panic hook.
    pub fn install(&self) {
        if self.crypto_provider {
            install_crypto_provider();
        }
        if self.suppress_core_dumps {
            suppress_core_dumps();
        }
        if let Some(level) = self.logging.as_deref() {
            logging::init(level);
        }
        if self.panic_hook {
            install_panic_hook();
        }
    }
}

/// Install the process-wide rustls crypto provider (aws-lc-rs, pma-rust Lock 2).
/// Idempotent: a second call is a no-op.
fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Best-effort core-dump suppression (pma-rust acceptance checklist).
fn suppress_core_dumps() {
    let _ = rlimit::setrlimit(rlimit::Resource::CORE, 0, 0);
}

/// Emit a structured error before the runtime aborts (pma-rust acceptance checklist).
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(%info, "lode panicked");
    }));
}
