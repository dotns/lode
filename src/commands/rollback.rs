//! `lode rollback` — request a downgrade to a known-good (or explicit) version.
//!
//! Purely local: write the app-owned `target` field in `state.json` (atomic
//! temp+rename via [`crate::state`]) and exit. A running lode polls the file's
//! mtime (design §7), sees the new `target` and rolls the child onto it. With
//! `--version` the caller picks the version; otherwise we fall back to the
//! `last_good` recorded by lode, erroring clearly when neither is available.

use std::io::Write;
use std::path::Path;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::state;

/// Set `state.json`'s `target` to `version` (or the recorded `last_good`) and
/// report it. Writes through a locked stdout handle (the `println!` macro is
/// denied workspace-wide).
pub(crate) fn run(cfg: &Config, version: Option<&str>) -> Result<()> {
    let target = set_target(&cfg.global.dir, version)?;

    let mut out = std::io::stdout().lock();
    writeln!(out, "lode rollback: target set to {target}")?;
    Ok(())
}

/// Read `state.json` (defaulting when absent), choose the rollback target and
/// atomically write it back — under the shared `state.json.lock` flock, so a
/// concurrent supervisor or app RMW can never lose (or be lost to) the write
/// (P2-14). Returns the chosen version.
///
/// The target is `version` when given, else the recorded `last_good` — picked
/// inside the locked edit so the fallback is read under the same lock as the
/// write; an [`Error::State`] is returned when neither is available (the
/// unchanged state rewritten in that case is a harmless no-op).
fn set_target(dir: &Path, version: Option<&str>) -> Result<String> {
    let path = dir.join("state.json");
    let mut chosen: Option<String> = None;
    state::locked_update(&path, |st| {
        let target = match version {
            Some(v) => Some(v.to_owned()),
            None => st.last_good.clone(),
        };
        if let Some(target) = &target {
            st.target = Some(target.clone());
        }
        chosen = target;
    })?;
    chosen.ok_or_else(|| {
        Error::State(
            "rollback: no --version given and no last_good recorded in state.json".to_owned(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty scratch data dir unique to this process + label.
    fn scratch(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lode-rollback-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn explicit_version_sets_target() {
        let dir = scratch("explicit");
        assert_eq!(set_target(&dir, Some("2.0.0")).unwrap(), "2.0.0");
        let state = state::read(&dir.join("state.json")).unwrap().unwrap();
        assert_eq!(state.target.as_deref(), Some("2.0.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_to_last_good() {
        let dir = scratch("lastgood");
        let seed = state::State {
            last_good: Some("1.4.2".to_owned()),
            ..state::State::default()
        };
        state::write(&dir.join("state.json"), &seed).unwrap();

        assert_eq!(set_target(&dir, None).unwrap(), "1.4.2");
        let state = state::read(&dir.join("state.json")).unwrap().unwrap();
        assert_eq!(state.target.as_deref(), Some("1.4.2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_version_and_no_last_good_errors() {
        let dir = scratch("neither");
        let err = set_target(&dir, None).unwrap_err();
        assert!(matches!(err, Error::State(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
