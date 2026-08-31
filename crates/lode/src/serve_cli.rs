//! The clap-bound `serve` wrapper: bare `lode` as a supervised service.
//!
//! A thin shim over [`lode_supervisor::serve_core`] — resolve config from the
//! parsed globals, install the OWNED (signal-hook) signal source with lode's
//! standard set, and drive the loop with the binary defaults (subreaper +
//! single-instance lock), re-resolving *from the globals* on a `lode.toml`-change
//! reload so the CLI/env layer keeps applying across reloads.

use std::process::ExitCode;

use lode_core::Result;
use lode_supervisor::{OwnedSignalSource, SuperviseOptions, serve_core};

use crate::cli::Globals;
use crate::config_cli;

/// Run the app as a supervised service (bare `lode`).
pub(crate) fn serve(globals: &Globals) -> Result<ExitCode> {
    let cfg = config_cli::resolve(globals)?;
    // Install the signal handlers ONCE (before any bootstrap work): resolve/install
    // and the runtime fetch may download for minutes, and as PID 1 an unhandled
    // SIGTERM is simply ignored — `docker stop` would hang until the SIGKILL.
    let mut signals = OwnedSignalSource::new(&cfg)?;
    serve_core(cfg, &mut signals, SuperviseOptions::owned(), |_cfg| {
        // A paused app whose `lode.toml` was edited: re-resolve from the globals
        // (CLI/env layer included) so a reload behaves exactly as the loader did.
        config_cli::resolve(globals)
    })
}
