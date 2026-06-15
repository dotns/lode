//! `lode status` — print the resolved config and (if present) `state.json`, then
//! exit. Secrets are never printed: header and trusted-key values are reported only
//! by count.

use std::io::Write;

use crate::config::Config;
use crate::error::Result;
use crate::{manifest, state};

/// Print a status summary for `cfg`. Writes through a locked stdout handle so the
/// `clippy::print_stdout` lint (the `println!` macro is denied) stays satisfied.
pub(crate) fn run(cfg: &Config) -> Result<()> {
    let mut out = std::io::stdout().lock();

    writeln!(out, "lode status")?;

    writeln!(out, "[global]")?;
    writeln!(out, "  app:        {}", cfg.global.app)?;
    writeln!(out, "  data_dir:   {}", cfg.global.data_dir.display())?;
    writeln!(out, "  log_level:  {}", cfg.global.log_level)?;

    writeln!(out, "[update]")?;
    writeln!(out, "  source:     {}", update_source(cfg))?;
    writeln!(out, "  github_api: {}", cfg.update.github_api)?;
    writeln!(out, "  asset:      {}", opt(cfg.update.asset.as_deref()))?;
    writeln!(out, "  channel:    {}", cfg.update.channel)?;
    writeln!(out, "  policy:     {:?}", cfg.update.policy)?;
    writeln!(out, "  interval:   {}s", cfg.update.check_interval)?;
    writeln!(out, "  keep:       {}", cfg.update.keep_versions)?;
    writeln!(out, "  pin:        {}", opt(cfg.update.pin.as_deref()))?;

    writeln!(out, "[http]")?;
    writeln!(out, "  headers:    {} configured", cfg.http.headers.len())?;

    writeln!(out, "[trust]")?;
    writeln!(out, "  signature:  {:?}", cfg.trust.require_signature)?;
    writeln!(
        out,
        "  keys:       {} configured",
        cfg.trust.trusted_keys.len()
    )?;
    writeln!(
        out,
        "  keys_file:  {}",
        opt(cfg.trust.trusted_keys_file.as_deref())
    )?;

    writeln!(out, "[command]")?;
    writeln!(out, "  run:        {}", opt(cfg.command.run.as_deref()))?;
    writeln!(out, "  exec:       {}", opt(cfg.command.exec.as_deref()))?;
    writeln!(out, "  workdir:    {}", cfg.command.workdir)?;

    writeln!(out, "[runtime]")?;
    writeln!(out, "  runtime:    {}", opt(cfg.runtime.runtime.as_deref()))?;
    writeln!(
        out,
        "  download:   {}",
        opt(cfg.runtime.download.as_deref())
    )?;

    writeln!(out, "[supervise]")?;
    writeln!(out, "  restart:    {:?}", cfg.supervise.restart)?;
    writeln!(out, "  readiness:  {:?}", cfg.supervise.readiness)?;
    writeln!(out, "  ready_to:   {}s", cfg.supervise.ready_timeout)?;
    writeln!(
        out,
        "  grace:      {}s (rollback window)",
        cfg.supervise.health_grace
    )?;
    writeln!(out, "  stop_to:    {}s", cfg.supervise.stop_timeout)?;
    writeln!(
        out,
        "  backoff:    {}..{} ms (max {} restarts; restart != off)",
        cfg.supervise.restart_backoff, cfg.supervise.restart_backoff_max, cfg.supervise.restart_max
    )?;
    writeln!(out, "  mode:       {:?}", cfg.supervise.restart_mode)?;
    writeln!(
        out,
        "  listen:     {}",
        opt(cfg.supervise.listen.as_deref())
    )?;

    writeln!(out, "[signals]")?;
    writeln!(out, "  forward:    {}", forward_set(cfg))?;
    writeln!(out, "  restart:    {}", opt(cfg.signals.restart.as_deref()))?;

    write_remote(&mut out, cfg)?;
    write_state(&mut out, cfg)?;
    Ok(())
}

/// Summarise the remote manifest (best-effort). Only attempted when a source is
/// configured; any network/parse error is noted, never fatal — `status` must work
/// offline. URLs/secrets are not printed (the http layer redacts its own logs).
fn write_remote(out: &mut impl Write, cfg: &Config) -> Result<()> {
    if cfg.update.manifest.is_none() && cfg.update.github.is_none() {
        return Ok(());
    }
    writeln!(out, "[remote]")?;
    match manifest::fetch(cfg) {
        Ok(m) => {
            writeln!(out, "  name:       {}", m.name)?;
            // The effective publisher-signature posture, surfaced prominently and
            // consistent with `require_signature` (no secrets/URLs).
            writeln!(
                out,
                "  trust:      {}",
                crate::install::manifest_trust_posture(cfg, &m)
            )?;
            match m.channels.get(&cfg.update.channel) {
                Some(c) => writeln!(
                    out,
                    "  latest:     {} (channel {})",
                    c.latest, cfg.update.channel
                )?,
                None => writeln!(
                    out,
                    "  latest:     (channel {} not in manifest)",
                    cfg.update.channel
                )?,
            }
            writeln!(out, "  versions:   {} available", m.versions.len())?;
        }
        Err(e) => writeln!(out, "  unavailable: {e}")?,
    }
    Ok(())
}

/// Read and print `state.json`, or note its absence.
fn write_state(out: &mut impl Write, cfg: &Config) -> Result<()> {
    let path = cfg.global.data_dir.join("state.json");
    writeln!(out, "[state] {}", path.display())?;
    match state::read(&path)? {
        Some(s) => writeln!(out, "{}", serde_json::to_string_pretty(&s)?)?,
        None => writeln!(out, "  (none)")?,
    }
    Ok(())
}

/// Describe the configured update source without leaking anything sensitive.
fn update_source(cfg: &Config) -> String {
    match (&cfg.update.manifest, &cfg.update.github) {
        (Some(url), _) => format!("manifest {url}"),
        (None, Some(repo)) => format!("github {repo}"),
        (None, None) => "(none)".to_owned(),
    }
}

/// Render the forward-signal set, falling back to the standard-set note.
fn forward_set(cfg: &Config) -> String {
    if cfg.signals.forward.is_empty() {
        "(standard set)".to_owned()
    } else {
        cfg.signals.forward.join(", ")
    }
}

/// Render an optional string, using `(unset)` for `None`.
fn opt(value: Option<&str>) -> &str {
    value.unwrap_or("(unset)")
}
