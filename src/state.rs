//! `state.json` — the runtime comms channel shared by lode and the app.
//!
//! lode owns most fields (`current`/`last_good`/`available`/`status`/…); the app
//! writes `target` and `restart_nonce` to request an upgrade or restart. The
//! `ready` field is co-owned: it carries the staged-update handshake (§8) — lode
//! prompts the app to prepare, the app acks when it is ready to cut over, then the
//! freshly-spawned version reports it can serve. Reads tolerate a missing file
//! (`Ok(None)`); writes are atomic (write a sibling temp file, then rename); and
//! read-modify-write cycles serialise on a sibling `state.json.lock` flock
//! ([`locked_update`] / [`locked_update_lenient`]) so concurrent writers cannot
//! lose each other's updates. See design §7.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use nix::fcntl::{Flock, FlockArg};
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

    // --- co-owned staged-update + readiness handshake (§8) ---
    /// The value is `{LODE_INSTANCE}-{phase}`, where the trailing phase digit drives
    /// the cut-over: the app reports it can serve with `-0`; on a staged update lode
    /// prompts the running app with `-1`; the app acks "prepared, cut over now" with
    /// `-2`. lode reads `-0`/`-2` and writes the `-1` prompt (clearing it at cut-over
    /// so the new spawn's `-0` is unambiguous). Only exercised under `readiness=state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ready: Option<String>,
}

/// Read `state.json` if it exists. Returns `Ok(None)` when the file is absent.
/// Strict: a corrupt file is an error — correct for CLI commands, where failing
/// loudly beats acting on garbage. Supervise-loop paths use [`read_lenient`].
pub(crate) fn read(path: &Path) -> Result<Option<State>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Lenient read for supervise-loop / boot paths: lode runs as PID 1, so a
/// corrupt or torn `state.json` must never propagate an error and kill the
/// supervisor (the corrupt file survives restarts on the volume, turning one
/// bad write into a crash-loop). A parse failure logs a warning and quarantines
/// the file — best-effort rename to `state.json.corrupt` — so the next write
/// starts clean while the evidence is preserved. An absent file is `None`; any
/// other I/O error is logged and also yields `None`.
pub(crate) fn read_lenient(path: &Path) -> Option<State> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "state.json unreadable; ignoring");
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(state) => Some(state),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "state.json corrupt; quarantining as state.json.corrupt"
            );
            let _ = std::fs::rename(path, quarantine_path(path));
            None
        }
    }
}

/// Sibling quarantine path for a corrupt `state.json` (`<name>.corrupt`).
fn quarantine_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| std::ffi::OsString::from("state.json"), ToOwned::to_owned);
    name.push(".corrupt");
    path.with_file_name(name)
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

/// Serialised read-modify-write for strict (CLI) paths: hold an exclusive
/// advisory `flock(2)` on the sibling `state.json.lock` across the whole
/// read → `edit` → atomic-write cycle, so concurrent lode RMWs (and any app
/// honouring the documented contract — docs/integration.md §2) can never lose
/// each other's field updates. Plain readers stay lock-free: the temp+rename
/// replacement already guarantees a complete snapshot. Strict like [`read`]
/// (a corrupt file is an error); returns the state as written.
pub(crate) fn locked_update(path: &Path, edit: impl FnOnce(&mut State)) -> Result<State> {
    let lock = lock_exclusive(path)?;
    let mut state = read(path)?.unwrap_or_default();
    edit(&mut state);
    write(path, &state)?;
    drop(lock); // hold through the rename so the next locker reads our write
    Ok(state)
}

/// Best-effort, lenient sibling of [`locked_update`] for supervise-loop / PID-1
/// paths, where nothing may take down lode (design §8): a lock failure (e.g. a
/// read-only disk) degrades to the unserialised RMW with a warning, a corrupt
/// `state.json` is quarantined by [`read_lenient`] and the edit starts from
/// defaults, and a write failure is logged and swallowed.
pub(crate) fn locked_update_lenient(path: &Path, edit: impl FnOnce(&mut State)) {
    let lock = match lock_exclusive(path) {
        Ok(lock) => Some(lock),
        Err(e) => {
            tracing::warn!(error = %e, "state.json.lock unavailable; updating without it");
            None
        }
    };
    let mut state = read_lenient(path).unwrap_or_default();
    edit(&mut state);
    if let Err(e) = write(path, &state) {
        tracing::warn!(error = %e, "state.json write failed; continuing without it");
    }
    drop(lock);
}

/// Take a blocking exclusive `flock(2)` on the sibling lock file
/// ([`lock_path`]), created on first use and never removed (removal would let
/// a concurrent locker open a different inode and defeat the exclusion). The
/// lock guards the RMW *cycle*, not the data file: `state.json` itself is
/// replaced by temp+rename on every write, so flocking it directly would pin
/// an inode the next writer swaps away. Blocking is deliberate — the critical
/// section is one read plus one atomic write (microseconds), well below the
/// supervisor's 200ms loop tick (P2-14 design note).
fn lock_exclusive(path: &Path) -> Result<Flock<File>> {
    // Append mode: the file's (empty) contents are irrelevant — only its inode
    // matters as the flock anchor — and append+create never truncates.
    let file = File::options()
        .create(true)
        .append(true)
        .open(lock_path(path))?;
    Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_, errno)| std::io::Error::from(errno).into())
}

/// Sibling advisory-lock path for `state.json` RMWs (`<name>.lock`).
fn lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| std::ffi::OsString::from("state.json"), ToOwned::to_owned);
    name.push(".lock");
    path.with_file_name(name)
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

    // --- locked_update (P2-14 RMW serialisation) ---

    #[test]
    fn locked_update_edits_and_returns_written_state() {
        let path = scratch("locked-basic");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(lock_path(&path));

        // Starts from defaults when the file is absent, creates the sibling lock.
        let st = locked_update(&path, |st| st.current = Some("1.4.2".to_owned())).unwrap();
        assert_eq!(st.current.as_deref(), Some("1.4.2"));
        assert_eq!(
            read(&path).unwrap().unwrap().current.as_deref(),
            Some("1.4.2")
        );
        assert!(lock_path(&path).is_file());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(lock_path(&path));
    }

    #[test]
    fn locked_update_serialises_racing_rmws() {
        const N: u64 = 100;
        let path = scratch("locked-race");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(lock_path(&path));

        // Two threads each bump the nonce N times through the locked RMW. Each
        // thread opens the lock file itself, so the flocks are on distinct open
        // file descriptions and genuinely exclude each other; without the lock
        // the read-modify-write windows overlap and bumps are lost.
        std::thread::scope(|s| {
            for _ in 0..2 {
                s.spawn(|| {
                    for _ in 0..N {
                        locked_update(&path, |st| {
                            st.restart_nonce = st.restart_nonce.saturating_add(1);
                        })
                        .unwrap();
                    }
                });
            }
        });
        assert_eq!(read(&path).unwrap().unwrap().restart_nonce, 2 * N);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(lock_path(&path));
    }

    #[test]
    fn locked_update_lenient_tolerates_corrupt_state() {
        let path = scratch("locked-lenient");
        let quarantined = quarantine_path(&path);
        let _ = std::fs::remove_file(&quarantined);
        let _ = std::fs::remove_file(lock_path(&path));
        std::fs::write(&path, b"{\"current\":").unwrap();

        // The strict variant errors; the lenient one quarantines + rebuilds.
        assert!(locked_update(&path, |_| {}).is_err());
        locked_update_lenient(&path, |st| st.restart_nonce = 7);
        assert_eq!(read(&path).unwrap().unwrap().restart_nonce, 7);
        assert!(quarantined.is_file());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&quarantined);
        let _ = std::fs::remove_file(lock_path(&path));
    }

    // --- read_lenient (PID 1 corruption tolerance) ---

    #[test]
    fn read_lenient_absent_is_none() {
        let path = scratch("lenient-absent");
        let _ = std::fs::remove_file(&path);
        assert!(read_lenient(&path).is_none());
    }

    #[test]
    fn read_lenient_valid_is_some() {
        let path = scratch("lenient-valid");
        let state = State {
            current: Some("1.4.2".to_owned()),
            ..State::default()
        };
        write(&path, &state).unwrap();

        let back = read_lenient(&path).unwrap();
        assert_eq!(back.current.as_deref(), Some("1.4.2"));
        // A valid file is NOT quarantined.
        assert!(path.is_file());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_lenient_corrupt_is_none_and_quarantined() {
        let path = scratch("lenient-corrupt");
        let quarantined = quarantine_path(&path);
        let _ = std::fs::remove_file(&quarantined);
        let garbage: &[u8] = b"{\"current\":";
        std::fs::write(&path, garbage).unwrap();

        // The strict read rejects it; the lenient read tolerates + quarantines.
        assert!(read(&path).is_err());
        assert!(read_lenient(&path).is_none());

        // Evidence preserved, original moved aside so the next write starts clean.
        assert!(!path.exists());
        assert_eq!(std::fs::read(&quarantined).unwrap(), garbage);
        assert!(read(&path).unwrap().is_none());

        let _ = std::fs::remove_file(&quarantined);
    }
}
