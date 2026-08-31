#![forbid(unsafe_code)]
//! lode — universal, single-binary update loader.
//!
//! Verifies integrity (sha256) + publisher identity (ed25519), then launches and
//! manages a packaged application as a supervised child process, with policy-driven
//! hot-updates and automatic rollback. See `docs/architecture.md`.
//!
//! This crate is the CLI: argument parsing (clap), the publisher/authoring
//! commands, and the process-global init the binary installs. Everything else
//! lives in the two library crates it drives — `lode-core` (config, manifest
//! resolution, verified download/install, the `Engine` facade) and
//! `lode-supervisor` (the supervise loop, readiness/stop handshakes, exec
//! passthrough). It is split into a thin binary (`src/main.rs`) and this library
//! so the CLI surface is reachable from tests (the ruff `src/{main,lib}.rs`
//! pattern). [`run`] is the single entry point.
//!
//! **Multi-call binary:** [`run`] dispatches on the program name (`argv[0]`).
//! Invoked as `lode` it is the loader — no subcommands, bare = start, `lode
//! <args>` = exec passthrough. Invoked as `lode-cli` (a symlink to the same
//! binary) it is the operator/publisher toolkit (`status`/`update`/… and
//! `keygen`/`sign`/`verify`/`manifest`/`init`).

mod authoring;
mod cli;
mod config_cli;
mod serve_cli;

use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;

use clap::Parser as _;
use lode_core::InitOptions;
use lode_core::commands;

use crate::cli::{LoaderCli, ToolCli, ToolCommand};

/// Parse the CLI, resolve configuration, and dispatch to the selected operation.
///
/// Dispatches on `argv[0]`: `lode-cli` runs the operator/publisher toolkit;
/// anything else runs the loader (bare = supervised service; `lode <args>` =
/// exec passthrough, which replaces this process and never returns).
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
fn run_loader() -> anyhow::Result<ExitCode> {
    let cli = LoaderCli::parse();
    init_post_parse(&cli.globals);

    if cli.args.is_empty() {
        // The supervised service re-resolves config on a `lode.toml`-change reload,
        // so it owns config loading (across reloads); pass the parsed globals.
        Ok(serve_cli::serve(&cli.globals)?)
    } else {
        // `exec` replaces this process on success, so the `Ok` arm is uninhabited.
        let cfg = config_cli::resolve(&cli.globals)?;
        match lode_supervisor::exec_passthrough(&cfg, &cli.args)? {}
    }
}

/// The `lode-cli` toolkit: management commands talk to a running instance via the
/// resolved config; publisher commands (keygen/sign/verify/manifest/init) are
/// self-contained and need no config.
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
        ToolCommand::Status => commands::status::run(&config_cli::resolve(&cli.globals)?)?,
        ToolCommand::Update { version } => {
            commands::update::run(&config_cli::resolve(&cli.globals)?, version.as_deref())?;
        }
        ToolCommand::Rollback { version } => {
            commands::rollback::run(&config_cli::resolve(&cli.globals)?, version.as_deref())?;
        }
        ToolCommand::Restart => commands::restart::run(&config_cli::resolve(&cli.globals)?)?,
        ToolCommand::Versions => commands::versions::run(&config_cli::resolve(&cli.globals)?)?,
        ToolCommand::Seed {
            app_bin,
            version,
            no_activate,
        } => {
            // Scaffold a sourceless config if the data dir has none, so seeding a
            // fresh dir doesn't trip the source-requiring starter scaffold; the
            // seeded file's name derives the scaffolded [command] launch command.
            config_cli::ensure_sourceless_toml(&cli.globals, Path::new(&app_bin))?;
            commands::seed::run(
                &config_cli::resolve(&cli.globals)?,
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
/// clap-free (it takes the already-resolved level string). The subscriber must be
/// up before [`config_cli::resolve`] so resolve errors are logged, hence the
/// lenient peek.
fn resolve_log_level(globals: &cli::Globals) -> String {
    globals
        .log_level
        .clone()
        .or_else(|| config_cli::peek_log_level(globals))
        .unwrap_or_else(|| "info".to_owned())
}

/// Post-parse phase for the binary: install the tracing subscriber (at the level
/// resolved from `globals`) then the panic hook — exactly the old
/// `init_logging` + `install_panic_hook` sequence, routed through [`InitOptions`].
fn init_post_parse(globals: &cli::Globals) {
    InitOptions::new()
        .log_level(resolve_log_level(globals))
        .panic_hook(true)
        .install();
}
