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

mod authoring;
mod cli;
mod commands;
mod config;
mod download;
mod error;
mod http;
mod idval;
mod install;
mod lock;
mod logging;
mod manifest;
mod state;
mod supervisor;
mod verify;

use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser as _;

use crate::cli::{LoaderCli, ToolCli, ToolCommand};

/// Parse the CLI, resolve configuration, and dispatch to the selected operation.
///
/// Dispatches on `argv[0]`: `lode-cli` runs the operator/publisher toolkit;
/// anything else runs the loader (bare = supervised service; `lode <args>` =
/// exec passthrough, which replaces this process and never returns).
pub fn run() -> Result<ExitCode> {
    install_crypto_provider();
    suppress_core_dumps();

    if invoked_as_tool() {
        run_tool()
    } else {
        run_loader()
    }
}

/// True when the binary was invoked under the `lode-cli` name (the symlink).
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
fn run_loader() -> Result<ExitCode> {
    let cli = LoaderCli::parse();
    logging::init(&cli.globals.log_level);
    install_panic_hook();

    let cfg = config::resolve(&cli.globals)?;
    if cli.args.is_empty() {
        Ok(supervisor::serve(&cfg)?)
    } else {
        // `exec` replaces this process on success, so the `Ok` arm is uninhabited.
        match supervisor::exec_passthrough(&cfg, &cli.args)? {}
    }
}

/// The `lode-cli` toolkit: management commands talk to a running instance via the
/// resolved config; publisher commands (keygen/sign/verify/manifest/init) are
/// self-contained and need no config.
fn run_tool() -> Result<ExitCode> {
    let cli = ToolCli::parse();
    logging::init(&cli.globals.log_level);
    install_panic_hook();

    match cli.command {
        // --- publisher / authoring (no config required) ---
        ToolCommand::Keygen { out } => authoring::keygen(out.as_deref())?,
        ToolCommand::Sign {
            artifact,
            version,
            key,
            key_env,
        } => authoring::sign(&artifact, &version, key.as_deref(), key_env.as_deref())?,
        ToolCommand::Verify {
            artifact,
            version,
            pubkey,
            sig,
        } => authoring::verify(&artifact, &version, &pubkey, &sig)?,
        ToolCommand::Manifest {
            artifact,
            version,
            url,
            entry,
            size,
            channel,
            key,
            into,
        } => authoring::manifest(
            cli.globals.app.as_deref().unwrap_or("app"),
            &artifact,
            &version,
            &url,
            entry.as_deref(),
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
            entry,
            no_activate,
        } => {
            // Scaffold a sourceless config if the data dir has none, so seeding a
            // fresh dir doesn't trip the source-requiring starter scaffold.
            config::ensure_sourceless_toml(&cli.globals)?;
            commands::seed::run(
                &config::resolve(&cli.globals)?,
                &app_bin,
                &version,
                entry.as_deref(),
                !no_activate,
            )?;
        }
    }
    Ok(ExitCode::SUCCESS)
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
