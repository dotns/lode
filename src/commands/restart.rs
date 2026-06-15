//! `lode restart` — ask a running service to restart its child process.
//!
//! Purely local: bump the app-owned `restart_nonce` in `state.json` (atomic
//! temp+rename via [`crate::state`]) and exit. A running lode polls the file's
//! mtime (design §7), sees the changed nonce and recycles the child. With no
//! service running the bumped nonce simply takes effect on the next start.

use std::io::Write;
use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::state;

/// Increment `restart_nonce` in `state.json` and report the new value. Writes
/// through a locked stdout handle (the `println!` macro is denied workspace-wide).
pub(crate) fn run(cfg: &Config) -> Result<()> {
    let nonce = bump_nonce(&cfg.global.data_dir)?;

    let mut out = std::io::stdout().lock();
    writeln!(out, "lode restart: requested (restart_nonce={nonce})")?;
    Ok(())
}

/// Read `state.json` (defaulting when absent), increment `restart_nonce` and
/// atomically write it back — under the shared `state.json.lock` flock, so a
/// concurrent supervisor or app RMW can never lose the bump (P2-14). Returns
/// the new nonce.
fn bump_nonce(data_dir: &Path) -> Result<u64> {
    let path = data_dir.join("state.json");
    let state = state::locked_update(&path, |st| {
        st.restart_nonce = st.restart_nonce.saturating_add(1);
    })?;
    Ok(state.restart_nonce)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty scratch data dir unique to this process + label.
    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lode-restart-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bump_from_absent_starts_at_one() {
        let dir = scratch("absent");
        assert_eq!(bump_nonce(&dir).unwrap(), 1);
        // Persisted, so a second bump continues from the stored value.
        assert_eq!(bump_nonce(&dir).unwrap(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bump_persists_to_state_json() {
        let dir = scratch("persist");
        bump_nonce(&dir).unwrap();
        let state = state::read(&dir.join("state.json")).unwrap().unwrap();
        assert_eq!(state.restart_nonce, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
