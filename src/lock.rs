//! Single-instance PID lock at `$DATA_DIR/lode.pid` (design §9).
//!
//! The lock is created with `O_EXCL` ([`File::create_new`]) so two lode instances
//! sharing a data dir can never both hold it. The file records the holder's pid
//! and app name. When the file already exists we probe the recorded pid with
//! `kill(pid, None)`: a live process means another instance owns it (we refuse to
//! start); `ESRCH` (or an unparsable file) means a crashed instance left a stale
//! lock, which we remove and reclaim. The returned [`LockGuard`] removes the file
//! on drop, so a normal exit (or a `?` unwinding out of `serve`) releases it.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;

use crate::error::{Error, Result};

/// RAII handle for the held PID lock; removes `lode.pid` on drop.
#[derive(Debug)]
pub(crate) struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Acquire the single-instance lock for `app` under `data_dir`.
///
/// Returns [`Error::Lock`] when another live lode already holds it. A stale lock
/// (holder dead, or file corrupt) is reclaimed transparently.
pub(crate) fn acquire(data_dir: &Path, app: &str) -> Result<LockGuard> {
    fs::create_dir_all(data_dir)?;
    let path = data_dir.join("lode.pid");

    // Two passes at most: the second only runs after we remove a stale lock, so a
    // genuinely contended lock fails fast rather than spinning.
    for attempt in 0..2 {
        match File::create_new(&path) {
            Ok(mut file) => {
                write!(file, "{}\n{app}\n", std::process::id())?;
                file.sync_all()?;
                return Ok(LockGuard { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if attempt == 0 && reclaim_if_stale(&path)? {
                    continue;
                }
                return Err(Error::Lock(already_held_message(&path)));
            }
            Err(e) => return Err(Error::Lock(format!("create {}: {e}", path.display()))),
        }
    }
    Err(Error::Lock(format!(
        "could not acquire {} after reclaiming a stale lock",
        path.display()
    )))
}

/// If the existing lock's holder is dead (or the file is unreadable/corrupt),
/// remove it and report `true` so the caller can retry. A live holder yields
/// `false` (we must not take over a running instance).
fn reclaim_if_stale(path: &Path) -> Result<bool> {
    match read_pid(path) {
        Some(pid) if process_alive(pid) => Ok(false),
        Some(pid) => {
            tracing::warn!(
                pid = pid.as_raw(),
                "removing stale lode.pid (holder is gone)"
            );
            remove_stale(path)
        }
        None => {
            tracing::warn!(path = %path.display(), "removing unreadable/corrupt lode.pid");
            remove_stale(path)
        }
    }
}

/// Remove a stale lock file, tolerating a concurrent removal (`NotFound`).
fn remove_stale(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(Error::Lock(format!("remove stale {}: {e}", path.display()))),
    }
}

/// Parse the holder pid from the first line of the lock file, if readable.
fn read_pid(path: &Path) -> Option<Pid> {
    let text = fs::read_to_string(path).ok()?;
    let raw: i32 = text.lines().next()?.trim().parse().ok()?;
    Some(Pid::from_raw(raw))
}

/// Liveness probe via signal 0: alive unless `kill` reports `ESRCH`.
fn process_alive(pid: Pid) -> bool {
    !matches!(kill(pid, None), Err(Errno::ESRCH))
}

/// Human-readable "already running" message, naming the holder pid when known.
fn already_held_message(path: &Path) -> String {
    read_pid(path).map_or_else(
        || format!("lock {} is held by another instance", path.display()),
        |pid| {
            format!(
                "another lode instance is already running (pid {}, lock {})",
                pid.as_raw(),
                path.display()
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lode-lock-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn acquire_creates_and_drop_releases() {
        let dir = scratch("create");
        let path = dir.join("lode.pid");
        {
            let _guard = acquire(&dir, "myapp").unwrap();
            assert!(path.is_file());
            let text = std::fs::read_to_string(&path).unwrap();
            let mut lines = text.lines();
            assert_eq!(
                lines.next().unwrap().trim().parse::<u32>().unwrap(),
                std::process::id()
            );
            assert_eq!(lines.next().unwrap(), "myapp");
        }
        assert!(!path.exists(), "drop must remove the lock file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_acquire_while_held_fails() {
        let dir = scratch("contended");
        let _guard = acquire(&dir, "myapp").unwrap();
        // The on-disk pid is our own (alive), so a second attempt is refused.
        let err = acquire(&dir, "myapp").unwrap_err();
        assert!(matches!(err, Error::Lock(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reclaims_stale_lock_from_dead_pid() {
        let dir = scratch("stale");
        // A high, almost-certainly-unused pid → kill reports ESRCH → stale.
        std::fs::write(dir.join("lode.pid"), "2000000000\noldapp\n").unwrap();
        let guard = acquire(&dir, "myapp").unwrap();
        let text = std::fs::read_to_string(dir.join("lode.pid")).unwrap();
        assert_eq!(
            text.lines().next().unwrap().trim().parse::<u32>().unwrap(),
            std::process::id()
        );
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reclaims_corrupt_lock() {
        let dir = scratch("corrupt");
        std::fs::write(dir.join("lode.pid"), "not-a-pid\n").unwrap();
        let guard = acquire(&dir, "myapp").unwrap();
        assert!(dir.join("lode.pid").is_file());
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn process_alive_tracks_self_and_dead_pid() {
        assert!(process_alive(Pid::from_raw(
            i32::try_from(std::process::id()).unwrap()
        )));
        assert!(!process_alive(Pid::from_raw(2_000_000_000)));
    }
}
