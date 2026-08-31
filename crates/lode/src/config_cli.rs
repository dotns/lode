//! The clap-bound half of configuration loading.
//!
//! `lode-core` owns the resolution itself (merge → validate) behind the clap-free
//! [`Overrides`] seam; this module is the projection of the binary's parsed
//! [`Globals`] onto that seam, plus the CLI-side file search and the first-run
//! starter scaffold. Precedence stays `CLI > env (LODE_*) > lode.toml > default`
//! (design §10) — clap folds env into each global flag, so `Overrides` carries
//! both.

use std::path::Path;

use lode_core::config::{
    self, Overrides, find_config_path, peek_log_level as peek_log_level_at, scaffold_starter_config,
};
use lode_core::{Config, Result};

use crate::cli::Globals;

/// Resolve the effective configuration from the parsed globals, `lode.toml` and
/// the design defaults.
///
/// Locating the file is CLI-side: an explicit `--config`/`LODE_CONFIG` wins, else
/// `$LODE_DIR/lode.toml`, else `./lode.toml`. With no file anywhere, a source
/// given via CLI/env still runs file-less; otherwise a starter config is
/// scaffolded and the returned error guides the operator (design §15).
pub(crate) fn resolve(cli: &Globals) -> Result<Config> {
    let path = find_config_path(cli.config.as_deref(), cli.dir.as_deref());
    if path.is_none() && cli.manifest.is_none() && cli.github.is_none() {
        let dir = cli.dir.as_deref().unwrap_or(lode_core::config::DEFAULT_DIR);
        return Err(scaffold_starter_config(&Path::new(dir).join("lode.toml")));
    }
    config::resolve_with(&overrides_from_globals(cli), path)
}

/// Cheap pre-logging peek at `[global].log_level`, so the tracing subscriber
/// (installed before [`resolve`]) can honour the TOML value. Lenient: a missing,
/// unreadable or malformed file yields `None`.
pub(crate) fn peek_log_level(cli: &Globals) -> Option<String> {
    peek_log_level_at(&find_config_path(
        cli.config.as_deref(),
        cli.dir.as_deref(),
    )?)
}

/// Write a sourceless `lode.toml` when the data dir has none, so `lode-cli seed`
/// works on a fresh dir (see [`lode_core::config::ensure_sourceless_toml`]).
pub(crate) fn ensure_sourceless_toml(cli: &Globals, seed_source: &Path) -> Result<()> {
    config::ensure_sourceless_toml(cli.dir.as_deref(), cli.app.as_deref(), seed_source)
}

/// Project the clap [`Globals`] (CLI flags + folded `LODE_*` env) onto the
/// clap-free [`Overrides`] seam — the only clap-bound entry into `lode-core`'s
/// merge. The enum-valued flags convert from their `*Arg` mirrors (see
/// [`crate::cli`]).
fn overrides_from_globals(cli: &Globals) -> Overrides {
    Overrides {
        log_level: cli.log_level.clone(),
        app: cli.app.clone(),
        dir: cli.dir.clone(),
        manifest: cli.manifest.clone(),
        github: cli.github.clone(),
        github_api: cli.github_api.clone(),
        asset: cli.asset.clone(),
        channel: cli.channel.clone(),
        policy: cli.policy.map(Into::into),
        interval: cli.interval,
        keep: cli.keep,
        pin: cli.pin.clone(),
        header: cli.header.clone(),
        credential_host: cli.credential_host.clone(),
        allow_insecure_http: cli.allow_insecure_http,
        require_signature: cli.require_signature.map(Into::into),
        trusted_keys: cli.trusted_keys.clone(),
        trusted_keys_file: cli.trusted_keys_file.clone(),
        run: cli.run.clone(),
        exec: cli.exec.clone(),
        runtime: cli.runtime.clone(),
        runtime_download: cli.runtime_download.clone(),
        runtime_version: cli.runtime_version.clone(),
        runtime_version_check: cli.runtime_version_check.clone(),
        restart: cli.restart.map(Into::into),
        restart_backoff: cli.restart_backoff,
        restart_backoff_max: cli.restart_backoff_max,
        restart_max: cli.restart_max,
        readiness: cli.readiness.map(Into::into),
        ready_timeout: cli.ready_timeout,
        health_grace: cli.health_grace,
        stop_timeout: cli.stop_timeout,
        restart_mode: cli.restart_mode.map(Into::into),
        listen: cli.listen.clone(),
        forward_signals: cli.forward_signals.clone(),
        restart_signal: cli.restart_signal.clone(),
    }
}
