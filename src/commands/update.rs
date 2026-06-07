//! `lode update` — fetch the manifest, resolve a target, then download, verify
//! and install it (design §5/§13).
//!
//! If a supervised instance is running (a live `state.pid`), the new version is
//! installed and requested as a hot-update by writing `state.target` — the
//! running lode polls `state.json` and applies it (§7). Otherwise this command
//! activates the version directly: flip the `current` symlink and record it in
//! `state.json`. Either way old versions are pruned per `keep_versions`.

use std::io::Write;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::{download, install, manifest, state};

/// Run `update`, installing `version` (or the channel latest when `None`).
pub(crate) fn run(cfg: &Config, version: Option<&str>) -> Result<()> {
    // Clear any interrupted downloads/staging before we begin.
    install::gc(cfg)?;

    let manifest = manifest::fetch(cfg)?;
    if manifest.name != cfg.global.app {
        return Err(Error::Manifest(format!(
            "manifest name {:?} does not match configured app {:?}",
            manifest.name, cfg.global.app
        )));
    }
    // Verify the catalog's publisher signature before trusting any of its pointers.
    install::verify_manifest_identity(cfg, &manifest)?;

    let target = manifest::resolve_target(
        &manifest,
        &cfg.update.channel,
        cfg.update.pin.as_deref(),
        version,
    )?;
    let entry = manifest::version_entry(&manifest, &target)?;
    check_min_lode(entry, &target)?;

    let asset_name = cfg.update.asset.as_deref().ok_or_else(|| {
        Error::Config(
            "no [update].asset configured — set the asset filename to install (source-adapters §3)"
                .to_owned(),
        )
    })?;
    let asset = manifest::select_asset(entry, asset_name)?;

    let (temp, sha256) =
        download::fetch_artifact(cfg, asset, &target, &manifest::allowed_hosts(cfg))?;
    install::install(cfg, &target, asset, &temp, &sha256)?;

    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "lode update: installed {target} ({})",
        manifest::format_from_name(&asset.name)
    )?;
    if let Some(notes) = entry.notes.as_deref() {
        writeln!(out, "  notes: {notes}")?;
    }

    let path = cfg.global.data_dir.join("state.json");
    let mut st = state::read(&path)?.unwrap_or_default();
    st.channel = Some(cfg.update.channel.clone());

    if running_instance(&st) {
        // Hand off to the running supervisor via the app-owned request channel.
        st.available = Some(target.clone());
        st.target = Some(target.clone());
        state::write(&path, &st)?;
        writeln!(
            out,
            "  a service is running — requested hot-update to {target}"
        )?;
    } else {
        install::switch_current(cfg, &target)?;
        st.current = Some(target.clone());
        if st.last_good.is_none() {
            st.last_good = Some(target.clone());
        }
        st.available = None;
        state::write(&path, &st)?;
        writeln!(out, "  activated {target} (current)")?;
    }

    install::prune(cfg, st.current.as_deref(), st.last_good.as_deref())?;
    Ok(())
}

/// Enforce a version's `min_lode` against this loader's version, if declared.
fn check_min_lode(entry: &manifest::VersionEntry, version: &str) -> Result<()> {
    let Some(min) = entry.min_lode.as_deref() else {
        return Ok(());
    };
    let required = semver::Version::parse(min)?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
    if current < required {
        return Err(Error::Manifest(format!(
            "version {version} requires lode >= {required}, but this loader is {current}"
        )));
    }
    Ok(())
}

/// Is a supervised instance currently running? True when `state.pid` names a live
/// process (a no-signal `kill` succeeds, design §13).
fn running_instance(st: &state::State) -> bool {
    st.pid.is_some_and(process_alive)
}

/// Liveness probe via signal 0 (`kill(pid, None)`): `Ok` if the process exists.
fn process_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Asset, VersionEntry};

    fn entry_with_min(min: Option<&str>) -> VersionEntry {
        VersionEntry {
            min_lode: min.map(ToOwned::to_owned),
            notes: None,
            assets: vec![Asset {
                name: "x".to_owned(),
                url: "http://127.0.0.1/x".to_owned(),
                sha256: "00".to_owned(),
                sig: None,
                key_id: None,
                entry: Some("x".to_owned()),
                size: None,
                auth: true,
            }],
        }
    }

    #[test]
    fn min_lode_absent_or_satisfied_ok() {
        assert!(check_min_lode(&entry_with_min(None), "1.0.0").is_ok());
        // This loader is 0.1.0; a 0.0.x requirement is satisfied.
        assert!(check_min_lode(&entry_with_min(Some("0.0.1")), "1.0.0").is_ok());
    }

    #[test]
    fn min_lode_too_new_rejected() {
        let err = check_min_lode(&entry_with_min(Some("99.0.0")), "1.0.0").unwrap_err();
        assert!(matches!(err, Error::Manifest(_)));
    }

    #[test]
    fn process_alive_for_self_and_dead_for_unused_pid() {
        // Our own pid is alive.
        assert!(process_alive(std::process::id()));
        // A pid that overflows i32 is rejected by the guard.
        assert!(!process_alive(u32::MAX));
        // A high but in-range pid is almost certainly unused → kill reports ESRCH.
        assert!(!process_alive(2_000_000_000));
    }

    #[test]
    fn running_instance_false_without_pid() {
        let st = state::State::default();
        assert!(!running_instance(&st));
    }
}
