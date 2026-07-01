#![forbid(unsafe_code)]
//! lode — universal, single-binary update loader.
//!
//! Verifies integrity (sha256) + publisher identity (ed25519), then launches and
//! manages a packaged application as a supervised child process, with policy-driven
//! hot-updates and automatic rollback. See `docs/architecture.md`.
//!
//! The crate is split into a thin binary (`src/main.rs`) and this library so the
//! CLI surface and modules are reachable from integration tests and doctests
//! (the ruff `src/{main,lib}.rs` pattern). [`run`] is the single entry point.
//!
//! **Multi-call binary:** [`run`] dispatches on the program name (`argv[0]`).
//! Invoked as `lode` it is the loader — no subcommands, bare = start, `lode
//! <args>` = exec passthrough. Invoked as `lode-cli` (a symlink to the same
//! binary) it is the operator/publisher toolkit (`status`/`update`/… and
//! `keygen`/`sign`/`verify`/`manifest`/`init`).

// Leaf modules (always compiled): reachable from `engine.rs` / `InitOptions`
// even under `--features engine`, so they stay ungated. `commands` stays here
// too — the `Engine` facade calls `commands::{status,update,rollback,versions,
// restart}`, so the module is live under `--features engine` (only its cli-only
// `seed` submodule is gated, in `commands/mod.rs`).
mod commands;
mod config;
mod download;
mod error;
mod http;
mod idval;
mod install;
mod logging;
mod manifest;
mod state;
mod verify;

// Feature-gated layers: the embeddable engine, the process supervisor, and the
// binary's CLI / publisher-authoring surface. `lock` sits at the engine layer —
// `commands/update.rs` (wrapped by the `Engine` facade) calls `lock::live_holder`
// to detect a running instance — and its supervisor-only acquire half is gated
// inside the module itself.
#[cfg(feature = "cli")]
mod authoring;
#[cfg(feature = "cli")]
mod cli;
#[cfg(feature = "engine")]
mod engine;
#[cfg(feature = "engine")]
mod lock;
#[cfg(feature = "supervisor")]
mod supervisor;

// `ExitCode`/`OsStr`/`Path` back only the cli entry points (`run`, the loader /
// tool dispatch, `invoked_as_tool`); nothing in the always-on library surface
// uses them, so they are cli-gated to keep `--features engine` warning-clean.
#[cfg(feature = "cli")]
use std::ffi::OsStr;
#[cfg(feature = "cli")]
use std::path::Path;
#[cfg(feature = "cli")]
use std::process::ExitCode;

#[cfg(feature = "cli")]
use clap::Parser as _;

#[cfg(feature = "cli")]
use crate::cli::{LoaderCli, ToolCli, ToolCommand};

pub use crate::config::{
    Command, Config, ConfigBuilder, Global, Http, Policy, Readiness, RequireSignature, RestartMode,
    RestartPolicy, Runtime, Signals, Supervise, Trust, Update,
};
#[cfg(feature = "engine")]
pub use crate::engine::{CheckResult, Engine};
pub use crate::error::{Error, Result};
// The embeddable supervisor surface: the signal-source seam (so a host can drive
// the supervise loop without lode owning process-global signal handling), the
// Globals-free entry points, and the ownership knobs.
#[cfg(feature = "supervisor")]
pub use crate::supervisor::{
    ChannelSignalSource, OwnedSignalSource, SignalSender, SignalSource, SuperviseOptions,
    Supervisor, exec_passthrough, serve_embedded, signal_channel,
};

/// Parse the CLI, resolve configuration, and dispatch to the selected operation.
///
/// Dispatches on `argv[0]`: `lode-cli` runs the operator/publisher toolkit;
/// anything else runs the loader (bare = supervised service; `lode <args>` =
/// exec passthrough, which replaces this process and never returns).
#[cfg(feature = "cli")]
pub fn run() -> anyhow::Result<ExitCode> {
    // Pre-parse phase: install the crypto provider and suppress core dumps
    // before any CLI handling — same effects, same order as before, now routed
    // through the opt-in [`InitOptions`] API.
    InitOptions::new()
        .crypto_provider(true)
        .suppress_core_dumps(true)
        .install();

    if invoked_as_tool() {
        run_tool()
    } else {
        run_loader()
    }
}

/// True when the binary was invoked under the `lode-cli` name (the symlink).
#[cfg(feature = "cli")]
fn invoked_as_tool() -> bool {
    std::env::args_os()
        .next()
        .as_deref()
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .is_some_and(|name| name == "lode-cli")
}

/// The loader: bare `lode` starts the supervised service; `lode <args>` forwards
/// `<args>` to the app via exec passthrough (replacing this process).
#[cfg(feature = "cli")]
fn run_loader() -> anyhow::Result<ExitCode> {
    let cli = LoaderCli::parse();
    init_post_parse(&cli.globals);

    if cli.args.is_empty() {
        // The supervised service re-resolves config on a `lode.toml`-change reload,
        // so it owns config loading (across reloads); pass the parsed globals.
        Ok(supervisor::serve(&cli.globals)?)
    } else {
        // `exec` replaces this process on success, so the `Ok` arm is uninhabited.
        let cfg = config::resolve(&cli.globals)?;
        match supervisor::exec_passthrough(&cfg, &cli.args)? {}
    }
}

/// The `lode-cli` toolkit: management commands talk to a running instance via the
/// resolved config; publisher commands (keygen/sign/verify/manifest/init) are
/// self-contained and need no config.
#[cfg(feature = "cli")]
fn run_tool() -> anyhow::Result<ExitCode> {
    let cli = ToolCli::parse();
    init_post_parse(&cli.globals);

    match cli.command {
        // --- publisher / authoring (no config required) ---
        ToolCommand::Keygen { out } => authoring::keygen(out.as_deref())?,
        ToolCommand::Sign {
            artifact,
            version,
            run,
            exec,
            key,
            key_env,
        } => authoring::sign(
            &artifact,
            &version,
            run.as_deref(),
            exec.as_deref(),
            key.as_deref(),
            key_env.as_deref(),
        )?,
        ToolCommand::Verify {
            artifact,
            version,
            run,
            exec,
            pubkey,
            sig,
        } => authoring::verify(
            &artifact,
            &version,
            run.as_deref(),
            exec.as_deref(),
            &pubkey,
            &sig,
        )?,
        ToolCommand::Manifest {
            artifact,
            version,
            url,
            run,
            exec,
            size,
            channel,
            key,
            into,
        } => authoring::manifest(
            cli.globals.app.as_deref().unwrap_or("app"),
            &artifact,
            &version,
            &url,
            run.as_deref(),
            exec.as_deref(),
            size,
            &channel,
            &key,
            into.as_deref(),
        )?,
        ToolCommand::ManifestSign { into, key } => authoring::manifest_sign(&into, &key)?,
        ToolCommand::Init { path } => authoring::init(path.as_deref())?,

        // --- management (resolve config to locate the instance) ---
        ToolCommand::Status => commands::status::run(&config::resolve(&cli.globals)?)?,
        ToolCommand::Update { version } => {
            commands::update::run(&config::resolve(&cli.globals)?, version.as_deref())?;
        }
        ToolCommand::Rollback { version } => {
            commands::rollback::run(&config::resolve(&cli.globals)?, version.as_deref())?;
        }
        ToolCommand::Restart => commands::restart::run(&config::resolve(&cli.globals)?)?,
        ToolCommand::Versions => commands::versions::run(&config::resolve(&cli.globals)?)?,
        ToolCommand::Seed {
            app_bin,
            version,
            no_activate,
        } => {
            // Scaffold a sourceless config if the data dir has none, so seeding a
            // fresh dir doesn't trip the source-requiring starter scaffold; the
            // seeded file's name derives the scaffolded [command] launch command.
            config::ensure_sourceless_toml(&cli.globals, Path::new(&app_bin))?;
            commands::seed::run(
                &config::resolve(&cli.globals)?,
                &app_bin,
                &version,
                !no_activate,
            )?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve the tracing level from the parsed CLI globals, with precedence
/// CLI/env (`--log-level`/`LODE_LOG_LEVEL`) > a lenient `lode.toml` peek > "info".
/// Kept on the clap-aware binary side so [`InitOptions`]'s logging step stays
/// clap-free (it takes the already-resolved level string). Matches the design
/// note on [`config::peek_log_level`]: the subscriber must be up before
/// `config::resolve` so resolve errors are logged, hence the lenient peek.
#[cfg(feature = "cli")]
fn resolve_log_level(globals: &cli::Globals) -> String {
    globals
        .log_level
        .clone()
        .or_else(|| config::peek_log_level(globals))
        .unwrap_or_else(|| "info".to_owned())
}

/// Post-parse phase for the binary: install the tracing subscriber (at the level
/// resolved from `globals`) then the panic hook — exactly the old
/// `init_logging` + `install_panic_hook` sequence, routed through [`InitOptions`].
#[cfg(feature = "cli")]
fn init_post_parse(globals: &cli::Globals) {
    InitOptions::new()
        .log_level(resolve_log_level(globals))
        .panic_hook(true)
        .install();
}

/// Opt-in installer for lode's process-global side effects.
///
/// The `lode`/`lode-cli` binary installs all four — the rustls crypto provider,
/// core-dump suppression, the global tracing subscriber, and the panic hook. A
/// library consumer embedding the engine/supervisor gets **none** of them
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
/// lode::InitOptions::new().crypto_provider(true).install();
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
