//! Single-instance PID lock at `$LODE_DIR/lode.pid` (design §9).
//!
//! The lock is created with `O_EXCL` ([`File::create_new`]) so two lode instances
//! sharing a data dir can never both hold it. The file records the holder's pid
//! and app name. When the file already exists we probe the recorded pid with
//! `kill(pid, None)`: a live process means another instance owns it (we refuse to
//! start); `ESRCH` (or an unparsable file) means a crashed instance left a stale
//! lock, which we remove and reclaim. A recorded pid equal to our *own* pid is
//! also stale: we cannot have written it (we don't hold the lock yet), so the
//! previous holder must have died with the file intact and we inherited its pid
//! (PID reuse — in particular lode running as PID 1 in a container that was
//! kill -9'd and restarted on a persistent volume). The returned [`LockGuard`]
//! removes the file on drop, so a normal exit (or a `?` unwinding out of
//! `serve`) releases it.

use std::fs;
use std::path::Path;

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;

// The acquire half (`acquire` + `LockGuard`) is supervisor-only — bare `lode`'s
// single-instance lock. Under `--features engine` only the read-only
// `live_holder` probe is live (it backs `commands/update.rs`'s running-instance
// detection), so its supervisor-exclusive imports are gated to keep the engine
// build warning-clean.
#[cfg(feature = "supervisor")]
use std::fs::File;
#[cfg(feature = "supervisor")]
use std::io::Write as _;
#[cfg(feature = "supervisor")]
use std::path::PathBuf;

#[cfg(feature = "supervisor")]
use crate::error::{Error, Result};

/// RAII handle for the held PID lock; removes `lode.pid` on drop.
#[cfg(feature = "supervisor")]
#[derive(Debug)]
pub(crate) struct LockGuard {
    path: PathBuf,
}

#[cfg(feature = "supervisor")]
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Acquire the single-instance lock for `app` under `dir`.
///
/// Returns [`Error::Lock`] when another live lode already holds it. A stale lock
/// (holder dead, file corrupt, or recording our own pid) is reclaimed
/// transparently.
#[cfg(feature = "supervisor")]
pub(crate) fn acquire(dir: &Path, app: &str) -> Result<LockGuard> {
    fs::create_dir_all(dir)?;
    let path = dir.join("lode.pid");

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

/// Read-only liveness probe for CLI paths (P2-15): the pid of a live *other*
/// lode instance currently holding the single-instance lock under `dir`,
/// if any. Applies [`acquire`]'s staleness rules without reclaiming anything:
/// an absent or unparsable file, a dead holder, or a file recording our OWN
/// pid (we don't hold the lock — a previous holder died and we inherited its
/// pid, see module docs) all mean "no live supervisor" (`None`).
pub(crate) fn live_holder(dir: &Path) -> Option<u32> {
    let pid = read_pid(&dir.join("lode.pid"))?;
    if Some(pid.as_raw()) == own_pid() || !process_alive(pid) {
        return None;
    }
    u32::try_from(pid.as_raw()).ok()
}

/// If the existing lock's holder is dead (or the file is unreadable/corrupt),
/// remove it and report `true` so the caller can retry. A live holder yields
/// `false` (we must not take over a running instance) — unless the recorded pid
/// is our own: we don't hold the lock yet, so the file can only be a leftover
/// from a dead holder whose pid we inherited (PID reuse / PID-1 restart), and
/// probing it with `kill` would always report "alive" because it is us.
#[cfg(feature = "supervisor")]
fn reclaim_if_stale(path: &Path) -> Result<bool> {
    match read_pid(path) {
        Some(pid) if Some(pid.as_raw()) == own_pid() => {
            tracing::warn!(
                pid = pid.as_raw(),
                "lode.pid records our own pid; a previous holder with the same pid must have \
                 died (PID reuse / PID-1 restart) — reclaiming"
            );
            remove_stale(path)
        }
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
#[cfg(feature = "supervisor")]
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

/// Our own pid as an `i32` (the lock file's pid width), `None` on overflow
/// (cannot happen on Linux, where pids fit in an `i32`).
fn own_pid() -> Option<i32> {
    i32::try_from(std::process::id()).ok()
}

/// Human-readable "already running" message, naming the holder pid when known.
#[cfg(feature = "supervisor")]
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
    fn refuses_lock_held_by_live_other_process() {
        let dir = scratch("contended");
        // A live process that is not us: spawn a sleeping child and record its pid.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        std::fs::write(dir.join("lode.pid"), format!("{}\notherapp\n", child.id())).unwrap();
        let result = acquire(&dir, "myapp");
        let _ = child.kill();
        let _ = child.wait();
        assert!(matches!(result, Err(Error::Lock(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reclaims_lock_recording_own_pid() {
        let dir = scratch("own-pid");
        // Simulate a kill -9'd PID-1 lode restarting on a persistent volume: the
        // leftover lock records the pid we now run as. Pre-fix, the liveness
        // probe saw "alive" (it was probing us) and acquire refused forever.
        std::fs::write(
            dir.join("lode.pid"),
            format!("{}\noldapp\n", std::process::id()),
        )
        .unwrap();
        let guard = acquire(&dir, "myapp").unwrap();
        let text = std::fs::read_to_string(dir.join("lode.pid")).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next().unwrap().trim().parse::<u32>().unwrap(),
            std::process::id()
        );
        assert_eq!(lines.next().unwrap(), "myapp");
        drop(guard);
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
    fn live_holder_reports_live_other_process() {
        let dir = scratch("holder-live");
        // A live process that is not us: a sleeping child whose pid we record.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        std::fs::write(dir.join("lode.pid"), format!("{}\notherapp\n", child.id())).unwrap();
        let holder = live_holder(&dir);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(holder, Some(child.id()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_holder_treats_own_pid_as_stale() {
        let dir = scratch("holder-own");
        // Our own pid in the file is a dead holder's leftover (PID reuse), not us
        // supervising — we are the CLI asking.
        std::fs::write(
            dir.join("lode.pid"),
            format!("{}\noldapp\n", std::process::id()),
        )
        .unwrap();
        assert_eq!(live_holder(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_holder_none_for_dead_pid_corrupt_or_absent_file() {
        let dir = scratch("holder-stale");
        // Absent file.
        assert_eq!(live_holder(&dir), None);
        // Dead recorded pid.
        std::fs::write(dir.join("lode.pid"), "2000000000\noldapp\n").unwrap();
        assert_eq!(live_holder(&dir), None);
        // Corrupt file.
        std::fs::write(dir.join("lode.pid"), "not-a-pid\n").unwrap();
        assert_eq!(live_holder(&dir), None);
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
