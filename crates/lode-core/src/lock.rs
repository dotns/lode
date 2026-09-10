//! Single-instance PID lock at `$LODE_DIR/lode.pid` (design §9).
//!
//! A nonblocking exclusive flock on the persistent `lode.pid.lock` file prevents
//! concurrent supervisors from acquiring the same data directory. The guard holds
//! it for the full run, and the kernel releases it even after a forced exit.
//! The sibling `lode.pid` is created with `O_EXCL` ([`File::create_new`]) and records
//! the holder's pid and app name. This also preserves exclusion of legacy
//! supervisors that only use PID files. When it exists we probe the recorded pid with
//! `kill(pid, None)`. On Linux/Android we also inspect `/proc/<pid>/exe`: an
//! unrelated executable or an exited process means the lock is stale even when
//! signal zero succeeds (PID reuse or an unreaped zombie). A live lode means
//! another instance owns it; a dead holder or invalid file is reclaimed.
//! Other Unix platforms and unavailable identity checks retain the conservative
//! signal-zero behavior. A recorded pid equal to our *own* pid is
//! also stale: we cannot have written it (we don't hold the lock yet), so the
//! previous holder must have died with the file intact and we inherited its pid
//! (PID reuse — in particular lode running as PID 1 in a container that was
//! kill -9'd and restarted on a persistent volume). The returned [`LockGuard`]
//! removes the PID file before releasing flock on drop. The advisory lock file
//! is never removed: concurrent openers must always lock the same inode.

use std::fs;
use std::path::Path;

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::kill;
use nix::unistd::Pid;

// The acquire half (`acquire` + `LockGuard`) is supervisor-only — bare `lode`'s
// single-instance lock. Under `--features engine` only the read-only
// `live_holder` probe is live (it backs `commands/update.rs`'s running-instance
// detection), so its supervisor-exclusive imports are gated to keep the engine
// build warning-clean.
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;

use crate::error::{Error, Result};

/// RAII handle for the held PID lock; removes `lode.pid` on drop.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
    _lock: Flock<File>,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Acquire the single-instance lock for `app` under `dir`.
///
/// Returns [`Error::Lock`] when another live lode already holds it. A stale lock
/// (holder dead or unrelated, file corrupt, or recording our own pid) is reclaimed
/// transparently.
pub fn acquire(dir: &Path, app: &str) -> Result<LockGuard> {
    fs::create_dir_all(dir)?;
    let path = dir.join("lode.pid");
    let lock_path = dir.join("lode.pid.lock");
    let file = File::options()
        .create(true)
        .append(true)
        .open(&lock_path)
        .map_err(|e| Error::Lock(format!("open {}: {e}", lock_path.display())))?;
    let lock = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, errno)| {
        if errno == Errno::EWOULDBLOCK {
            Error::Lock(already_held_message(&path))
        } else {
            Error::Lock(format!("lock {}: {errno}", lock_path.display()))
        }
    })?;

    // Two passes at most: the second only runs after we remove a stale lock, so a
    // genuinely contended lock fails fast rather than spinning.
    for attempt in 0..2 {
        match File::create_new(&path) {
            Ok(mut file) => {
                let guard = LockGuard { path, _lock: lock };
                write!(file, "{}\n{app}\n", std::process::id())?;
                file.sync_all()?;
                return Ok(guard);
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
/// if any.
///
/// Applies [`acquire`]'s staleness rules without reclaiming anything:
/// an absent or unparsable file, a dead or unrelated holder, or a file recording our OWN
/// pid (we don't hold the lock — a previous holder died and we inherited its
/// pid, see module docs) all mean "no live supervisor" (`None`).
pub fn live_holder(dir: &Path) -> Option<u32> {
    let pid = read_pid(&dir.join("lode.pid"))?;
    if Some(pid.as_raw()) == own_pid() || !process_is_lode(pid) {
        return None;
    }
    u32::try_from(pid.as_raw()).ok()
}

/// If the existing lock's holder is dead or unrelated (or the file is unreadable/corrupt),
/// remove it and report `true` so the caller can retry. A live lode yields
/// `false` (we must not take over a running instance) — unless the recorded pid
/// is our own: we don't hold the lock yet, so the file can only be a leftover
/// from a dead holder whose pid we inherited (PID reuse / PID-1 restart), and
/// probing it with `kill` would always report "alive" because it is us.
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
        Some(pid) if process_is_lode(pid) => Ok(false),
        Some(pid) => {
            tracing::warn!(
                pid = pid.as_raw(),
                "removing stale lode.pid (holder is gone or PID belongs to another executable)"
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
    (raw > 0).then(|| Pid::from_raw(raw))
}

/// Check the executable as well as liveness so PID reuse does not impersonate lode.
fn process_is_lode(pid: Pid) -> bool {
    if !process_alive(pid) {
        return false;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // current_exe also needs procfs. If it is unavailable, do not reclaim a
        // potentially live holder merely because procfs is not mounted.
        let Ok(current) = std::env::current_exe() else {
            return true;
        };
        match fs::read_link(format!("/proc/{}/exe", pid.as_raw())) {
            Ok(executable) => executable_is_lode(&executable, &current),
            // Exited processes (including zombies) no longer have an executable.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                tracing::warn!(
                    pid = pid.as_raw(),
                    %error,
                    "cannot inspect lock holder executable; retaining lode.pid"
                );
                true
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        true
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn executable_is_lode(executable: &Path, current: &Path) -> bool {
    // A renamed or embedded supervisor shares our executable path. Distributed
    // lode binaries can also run from another installation directory.
    let executable = without_deleted_suffix(executable);
    executable == without_deleted_suffix(current)
        || executable
            .file_name()
            .is_some_and(|name| name == "lode" || name == "lode-cli")
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn without_deleted_suffix(path: &Path) -> &Path {
    use std::os::unix::ffi::OsStrExt as _;

    // Self-update replaces the file while the old executable remains mapped.
    let bytes = path.as_os_str().as_bytes();
    Path::new(std::ffi::OsStr::from_bytes(
        bytes.strip_suffix(b" (deleted)").unwrap_or(bytes),
    ))
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

    struct Child(std::process::Child);

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn holder_child(dir: &Path) -> Child {
        spawn_holder_child(dir, false)
    }

    fn spawn_holder_child(dir: &Path, legacy: bool) -> Child {
        let child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", "lock::tests::hold_lock_in_child"])
                .env("LODE_LOCK_TEST_DIR", dir)
                .env("LODE_LOCK_TEST_LEGACY", if legacy { "1" } else { "0" })
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while read_pid(&dir.join("lode.pid")).map(|pid| pid.as_raw().cast_unsigned())
            != Some(child.0.id())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "child did not acquire lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn hold_lock_in_child() {
        let dir = PathBuf::from(std::env::var_os("LODE_LOCK_TEST_DIR").unwrap());
        let _guard = if std::env::var("LODE_LOCK_TEST_LEGACY").unwrap() == "1" {
            std::fs::write(
                dir.join("lode.pid"),
                format!("{}\nmyapp\n", std::process::id()),
            )
            .unwrap();
            None
        } else {
            Some(acquire(&dir, "myapp").unwrap())
        };
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
    }

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
        let child = holder_child(&dir);
        let result = acquire(&dir, "myapp");
        assert!(matches!(result, Err(Error::Lock(_))));
        assert_eq!(live_holder(&dir), Some(child.0.id()));
        drop(child);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_second_acquisition_in_same_process() {
        let dir = scratch("same-process");
        let guard = acquire(&dir, "myapp").unwrap();
        let original = std::fs::read_to_string(dir.join("lode.pid")).unwrap();
        assert!(matches!(acquire(&dir, "otherapp"), Err(Error::Lock(_))));
        assert_eq!(
            std::fs::read_to_string(dir.join("lode.pid")).unwrap(),
            original
        );
        drop(guard);
        let guard = acquire(&dir, "myapp").unwrap();
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn protects_live_holder_when_pid_metadata_is_corrupt_or_missing() {
        let dir = scratch("damaged-metadata");
        let child = holder_child(&dir);
        let path = dir.join("lode.pid");
        std::fs::write(&path, "not-a-pid\n").unwrap();
        assert!(matches!(acquire(&dir, "otherapp"), Err(Error::Lock(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not-a-pid\n");
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(acquire(&dir, "otherapp"), Err(Error::Lock(_))));
        assert!(!path.exists());
        drop(child);
        let guard = acquire(&dir, "myapp").unwrap();
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn advisory_lock_file_survives_release_and_restart() {
        use std::os::unix::fs::MetadataExt as _;

        let dir = scratch("stable-inode");
        let path = dir.join("lode.pid.lock");
        let guard = acquire(&dir, "myapp").unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        drop(guard);
        assert!(!dir.join("lode.pid").exists());
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        let guard = acquire(&dir, "myapp").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn respects_legacy_pid_only_holder() {
        let dir = scratch("legacy-holder");
        let child = spawn_holder_child(&dir, true);
        assert!(!dir.join("lode.pid.lock").exists());
        let original = std::fs::read_to_string(dir.join("lode.pid")).unwrap();
        for _ in 0..2 {
            assert!(matches!(acquire(&dir, "otherapp"), Err(Error::Lock(_))));
            assert_eq!(live_holder(&dir), Some(child.0.id()));
            assert_eq!(
                std::fs::read_to_string(dir.join("lode.pid")).unwrap(),
                original
            );
        }
        drop(child);
        let guard = acquire(&dir, "myapp").unwrap();
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acquisition_error_releases_kernel_lock() {
        let dir = scratch("acquire-error");
        let path = dir.join("lode.pid");
        std::fs::create_dir(&path).unwrap();
        assert!(acquire(&dir, "myapp").is_err());
        std::fs::remove_dir(&path).unwrap();
        let guard = acquire(&dir, "myapp").unwrap();
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reclaims_pid_reused_by_unrelated_process() {
        use std::os::unix::process::CommandExt as _;

        let dir = scratch("reused-pid");
        // argv[0] is not proof of identity: inspect the actual executable.
        let child = Child(
            std::process::Command::new("sleep")
                .arg0("lode")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        std::fs::write(dir.join("lode.pid"), format!("{}\nmyapp\n", child.0.id())).unwrap();
        let guard = acquire(&dir, "myapp").unwrap();
        assert!(process_alive(Pid::from_raw(
            i32::try_from(child.0.id()).unwrap()
        )));
        drop(guard);
        drop(child);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reclaims_lock_after_holder_is_killed() {
        let dir = scratch("killed-holder");
        let mut child = holder_child(&dir);
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(
            dir.join("lode.pid").exists(),
            "forced exit leaves the PID file"
        );
        assert_eq!(live_holder(&dir), None);
        let guard = acquire(&dir, "myapp").unwrap();
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reclaims_lock_from_unreaped_holder() {
        use nix::sys::wait::{Id, WaitPidFlag, waitid};

        let dir = scratch("zombie-holder");
        let mut child = holder_child(&dir);
        let pid = Pid::from_raw(i32::try_from(child.0.id()).unwrap());
        child.0.kill().unwrap();
        waitid(Id::Pid(pid), WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT).unwrap();
        // Signal zero still succeeds for a zombie, but it no longer runs lode.
        assert!(process_alive(pid));
        assert_eq!(live_holder(&dir), None);
        let guard = acquire(&dir, "myapp").unwrap();
        drop(guard);
        drop(child);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reclaims_invalid_nonpositive_pids() {
        let dir = scratch("invalid-pid");
        for pid in [0, -1] {
            std::fs::write(dir.join("lode.pid"), format!("{pid}\nmyapp\n")).unwrap();
            assert_eq!(live_holder(&dir), None);
            let guard = acquire(&dir, "myapp").unwrap();
            drop(guard);
        }
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
        let child = holder_child(&dir);
        let holder = live_holder(&dir);
        assert_eq!(holder, Some(child.0.id()));
        drop(child);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn live_holder_ignores_unrelated_process() {
        let dir = scratch("holder-unrelated");
        let child = Child(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let path = dir.join("lode.pid");
        let original = format!("{}\nmyapp\n", child.0.id());
        std::fs::write(&path, &original).unwrap();
        assert_eq!(live_holder(&dir), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        drop(child);
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

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn executable_identity_handles_installations_renames_and_updates() {
        let current = Path::new("/app/custom-loader");
        for executable in [
            "/app/custom-loader",
            "/app/custom-loader (deleted)",
            "/other/install/lode",
            "/other/install/lode (deleted)",
            "/other/install/lode-cli",
        ] {
            assert!(executable_is_lode(Path::new(executable), current));
        }
        for executable in [
            "/usr/bin/sleep",
            "/usr/bin/sleep (deleted)",
            "/app/lode-helper",
        ] {
            assert!(!executable_is_lode(Path::new(executable), current));
        }
        assert!(executable_is_lode(
            current,
            Path::new("/app/custom-loader (deleted)")
        ));
    }
}
