//! `lode-cli seed <app-bin>` — dev/testing: install a LOCAL executable (or archive)
//! as a version without a manifest, download, or verification, and (by default)
//! activate it. After seeding, bare `lode --data-dir <dir>` supervises it fully
//! offline. This deliberately bypasses the sha256 + signature checks of the real
//! install path: you are placing trusted bytes yourself. Not for production.

use std::io::Write;
use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::{install, manifest};

/// Seed `app_bin` as `version` into the configured data dir (activating it unless
/// `activate` is false), then print where it landed and how to run it.
pub(crate) fn run(
    cfg: &Config,
    app_bin: &str,
    version: &str,
    entry: Option<&str>,
    activate: bool,
) -> Result<()> {
    install::seed_local(cfg, version, Path::new(app_bin), entry, activate)?;

    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "lode seed: installed {version} from {app_bin} ({})",
        manifest::format_from_name(
            Path::new(app_bin)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(app_bin)
        )
    )?;
    if activate {
        writeln!(out, "  activated {version} (current)")?;
    }
    writeln!(
        out,
        "  run it: lode --data-dir {} (offline, no source needed)",
        cfg.global.data_dir.display()
    )?;
    Ok(())
}
