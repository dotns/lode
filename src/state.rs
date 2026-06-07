//! `state.json` — the runtime comms channel shared by lode and the app.
//!
//! lode owns most fields (`current`/`last_good`/`available`/`status`/…); the app
//! writes `target` and `restart_nonce` to request an upgrade or restart. Reads
//! tolerate a missing file (`Ok(None)`); writes are atomic (write a sibling temp
//! file, then rename). See design §7.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Lifecycle status lode reports in `state.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Status {
    Starting,
    Running,
    Updating,
    RollingBack,
    Stopping,
    Stopped,
    Error,
}

/// Outcome of a version recorded in the rollout `history`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum HistoryResult {
    Good,
    Bad,
}

/// One entry in the rollout history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HistoryEntry {
    pub(crate) version: String,
    pub(crate) at: String,
    pub(crate) result: HistoryResult,
}

/// Contents of `$DATA_DIR/state.json`. Fields default to empty so a partial file
/// (or one written only by the app) still deserialises.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    // --- lode-owned ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) current: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_good: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) available: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) history: Vec<HistoryEntry>,

    // --- app-owned (requests) ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    #[serde(default)]
    pub(crate) restart_nonce: u64,
    /// Readiness handshake (§8): the app writes its spawn's `LODE_INSTANCE` here
    /// once it can serve. lode reads (never writes) it to gate `readiness=state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ready: Option<String>,
}

/// Read `state.json` if it exists. Returns `Ok(None)` when the file is absent.
pub(crate) fn read(path: &Path) -> Result<Option<State>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Atomically write `state.json`: serialise, write a sibling temp file in the
/// same directory, then rename over the target (rename is atomic within a fs).
pub(crate) fn write(path: &Path, state: &State) -> Result<()> {
    let json = serde_json::to_vec_pretty(state)?;
    let tmp = temp_path(path);
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Last-modified time of `state.json`, or `None` if it is absent. lode polls
/// this to notice app-written requests without an out-of-band signal (§7).
pub(crate) fn mtime(path: &Path) -> Result<Option<SystemTime>> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(meta.modified()?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A unique sibling temp path next to `path`, kept in the same directory so the
/// final `rename` stays on one filesystem (and thus atomic).
fn temp_path(path: &Path) -> PathBuf {
    let pid = std::process::id();
    let mut name = path
        .file_name()
        .map_or_else(|| std::ffi::OsString::from("state.json"), ToOwned::to_owned);
    name.push(format!(".{pid}.tmp"));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lode-state-{}-{label}.json", std::process::id()))
    }

    #[test]
    fn read_absent_is_none() {
        let path = scratch("absent");
        let _ = std::fs::remove_file(&path);
        assert!(read(&path).unwrap().is_none());
        assert!(mtime(&path).unwrap().is_none());
    }

    #[test]
    fn write_then_read_roundtrips() {
        let path = scratch("roundtrip");
        let state = State {
            current: Some("1.4.2".to_owned()),
            last_good: Some("1.4.2".to_owned()),
            status: Some(Status::Running),
            pid: Some(12_345),
            restart_nonce: 7,
            ready: Some("4242-3".to_owned()),
            history: vec![HistoryEntry {
                version: "1.4.2".to_owned(),
                at: "2026-06-04T21:00:00Z".to_owned(),
                result: HistoryResult::Good,
            }],
            ..State::default()
        };
        write(&path, &state).unwrap();

        let back = read(&path).unwrap().unwrap();
        assert_eq!(back.current.as_deref(), Some("1.4.2"));
        assert_eq!(back.status, Some(Status::Running));
        assert_eq!(back.pid, Some(12_345));
        assert_eq!(back.restart_nonce, 7);
        // The app-owned `ready` field round-trips so lode never clobbers it.
        assert_eq!(back.ready.as_deref(), Some("4242-3"));
        assert_eq!(back.history.len(), 1);
        assert_eq!(back.history[0].result, HistoryResult::Good);
        assert!(mtime(&path).unwrap().is_some());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn status_serialises_kebab_case() {
        let json = serde_json::to_string(&Status::RollingBack).unwrap();
        assert_eq!(json, "\"rolling-back\"");
    }
}
