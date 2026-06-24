//! Single-file Rust SDK for the `lode` supervisor (github.com/dotns/lode).
//! Wraps the state.json contract: read status, request upgrade/restart/rollback,
//! report readiness, subscribe to lode's notifications. The SDK only *signals* lode
//! (writes target/restart_nonce/ready under state.json.lock); lode does the heavy
//! fetch→verify→install→observe. Requires serde + serde_json; Unix only.
//! Drop in as `mod lode;`. Contract: ../docs/integration.md §2.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::os::raw::c_int;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Boxed error, so the single file stays light yet ergonomic with `?`.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

// flock(2) / signal(2): std exposes neither, so declare the two libc symbols we
// need (mirrors tests/apps/web-rust's `signal` shim).
extern "C" {
    fn flock(fd: c_int, op: c_int) -> c_int;
    fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> *const ();
}
const LOCK_EX: c_int = 2;
const LOCK_UN: c_int = 8;
const SIGINT: c_int = 2;
const SIGTERM: c_int = 15;

/// Lifecycle status lode reports (kebab-case on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Starting,
    Running,
    Held,
    Updating,
    RollingBack,
    Stopping,
    Stopped,
    Error,
}

/// One entry in lode's rollout history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub version: String,
    pub at: String,
    pub result: String, // "good" | "bad"
}

/// Parsed state.json. lode writes the top group; the app writes
/// `target`/`restart_nonce`/`ready`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_good: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<HistoryEntry>,
    #[serde(default)]
    pub config_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub restart_nonce: u64,
    #[serde(default)]
    pub hold: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<String>,
}

/// A handle on one lode data directory. [`Lode::from_env`] for the supervised app;
/// [`Lode::new`] for an external tool.
pub struct Lode {
    lode_dir: PathBuf,
    instance: String,
}

impl Lode {
    /// For an explicit data dir and instance id. `instance` may be empty when you
    /// only issue requests (target / restart) and never report readiness.
    pub fn new(lode_dir: impl Into<PathBuf>, instance: impl Into<String>) -> Self {
        Self {
            lode_dir: lode_dir.into(),
            instance: instance.into(),
        }
    }

    /// From the injected env (`LODE_DIR` / `LODE_INSTANCE`).
    pub fn from_env() -> Result<Self> {
        let dir = std::env::var_os("LODE_DIR")
            .ok_or("lode: LODE_DIR not set — run under lode, or use Lode::new")?;
        Ok(Self {
            lode_dir: PathBuf::from(dir),
            instance: std::env::var("LODE_INSTANCE").unwrap_or_default(),
        })
    }

    /// lode's directory this handle targets (where state.json lives).
    pub fn lode_dir(&self) -> &Path {
        &self.lode_dir
    }

    /// This launch's unique id (empty when not supervised).
    pub fn instance(&self) -> &str {
        &self.instance
    }

    fn state_path(&self) -> PathBuf {
        self.lode_dir.join("state.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.lode_dir.join("state.json.lock")
    }

    /// Parse `state.json` (`Ok(None)` when absent). Lock-free — atomic rename
    /// guarantees a whole snapshot.
    pub fn read(&self) -> Result<Option<State>> {
        match fs::read(self.state_path()) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Locked RMW primitive: `edit` mutates the raw object (snake_case keys);
    /// unknown keys round-trip verbatim. The request/readiness helpers wrap it.
    pub fn update<F: FnOnce(&mut Map<String, Value>)>(&self, edit: F) -> Result<State> {
        let _guard = LockGuard::acquire(&self.lock_path()); // best-effort flock(2)

        let mut obj: Map<String, Value> = match fs::read(self.state_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(), // corrupt → {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => Map::new(),
            Err(e) => return Err(e.into()),
        };

        edit(&mut obj);

        let value = Value::Object(obj);
        let mut json = serde_json::to_vec_pretty(&value)?;
        json.push(b'\n');
        let tmp = self.temp_path();
        fs::write(&tmp, &json)?;
        fs::rename(&tmp, self.state_path())?;
        Ok(serde_json::from_value(value)?)
    }

    /// Ask lode to restart your own process — a clean graceful stop (SIGTERM) +
    /// respawn of the current version. Use to self-recycle (you detected a resource
    /// leak, or on a periodic schedule), or to apply a lode.toml/[env] edit (the
    /// Run-phase restart re-reads lode.toml). Bumps `restart_nonce`; lode acts ~1s
    /// later, once per bump. Returns the new nonce.
    pub fn reboot(&self) -> Result<u64> {
        let st = self.update(|o| {
            let next = o.get("restart_nonce").and_then(Value::as_u64).unwrap_or(0) + 1;
            o.insert("restart_nonce".into(), Value::from(next));
        })?;
        Ok(st.restart_nonce)
    }

    /// Apply a pending lode.toml edit — alias of [`Self::reboot`] (the restart
    /// re-reads lode.toml).
    pub fn reload_config(&self) -> Result<u64> {
        self.reboot()
    }

    /// Set `target` (a version or `"latest"`) to request an up/down-grade.
    pub fn request_update(&self, version: &str) -> Result<()> {
        if version.is_empty() {
            return Err("lode: request_update needs a non-empty version".into());
        }
        self.update(|o| {
            o.insert("target".into(), Value::from(version));
        })?;
        Ok(())
    }

    /// Ask lode NOT to (re)start your process (maintenance) → `status = held`; a
    /// running child is left alone. Clear with [`Self::release`].
    pub fn hold(&self) -> Result<()> {
        self.update(|o| {
            o.insert("hold".into(), Value::Bool(true));
        })?;
        Ok(())
    }

    /// Clear a hold (see [`Self::hold`]) → lode resumes (re)starting your process.
    pub fn release(&self) -> Result<()> {
        self.update(|o| {
            o.insert("hold".into(), Value::Bool(false));
        })?;
        Ok(())
    }

    /// Roll back to `version`, else to the recorded `last_good`. Returns the chosen
    /// version, or an error if neither exists.
    pub fn rollback(&self, version: Option<&str>) -> Result<String> {
        let mut chosen: Option<String> = version.map(str::to_owned);
        self.update(|o| {
            if chosen.is_none() {
                chosen = o
                    .get("last_good")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            if let Some(t) = &chosen {
                o.insert("target".into(), Value::from(t.clone()));
            }
        })?;
        chosen.ok_or_else(|| "lode: rollback needs a version or a recorded last_good".into())
    }

    /// Report "I can serve" with the bare token. Use unless you opt into the phased handshake.
    pub fn mark_ready(&self) -> Result<()> {
        let token = self.require_instance()?.to_owned();
        self.set_ready(token)
    }

    /// Phased handshake: report serving as `"{instance}-0"`.
    pub fn mark_serving(&self) -> Result<()> {
        let token = format!("{}-0", self.require_instance()?);
        self.set_ready(token)
    }

    /// Phased handshake: ack "prepared, cut over" as `"{instance}-2"`.
    pub fn ack_prepared(&self) -> Result<()> {
        let token = format!("{}-2", self.require_instance()?);
        self.set_ready(token)
    }

    /// Is lode prompting THIS instance to prepare (`ready` == `"{instance}-1"`)?
    pub fn prepare_requested(&self, state: &State) -> bool {
        !self.instance.is_empty()
            && state.ready.as_deref() == Some(format!("{}-1", self.instance).as_str())
    }

    /// Poll `state.json` every `interval` (min 1s) until `stop` is set, firing
    /// `handlers`' callbacks on change. Blocking — run it on its own thread.
    pub fn watch(&self, interval: Duration, stop: &AtomicBool, mut handlers: Handlers<'_>) {
        let interval = if interval.is_zero() {
            Duration::from_secs(1)
        } else {
            interval
        };
        let seed = self.read().ok().flatten();
        let mut gen = seed.as_ref().map_or(0, |s| s.config_generation);
        let mut status = seed.as_ref().and_then(|s| s.status);
        let mut available = seed.as_ref().and_then(|s| s.available.clone());
        let mut last_error = seed.as_ref().and_then(|s| s.last_error.clone());
        let mut current = seed.as_ref().and_then(|s| s.current.clone());
        let mut last_good = seed.as_ref().and_then(|s| s.last_good.clone());
        let mut hold = seed.as_ref().is_some_and(|s| s.hold);
        let mut prompted = false;
        while !stop.load(Ordering::SeqCst) {
            if let Ok(Some(s)) = self.read() {
                if let Some(cb) = handlers.on_state.as_mut() {
                    cb(&s);
                }
                if s.config_generation > gen {
                    gen = s.config_generation;
                    if let Some(cb) = handlers.on_config_change.as_mut() {
                        cb(gen, &s);
                    }
                }
                if s.available != available {
                    available = s.available.clone();
                    if let (Some(v), Some(cb)) =
                        (s.available.as_deref(), handlers.on_available.as_mut())
                    {
                        cb(v, &s);
                    }
                }
                if s.status != status {
                    status = s.status;
                    if let (Some(st), Some(cb)) = (s.status, handlers.on_status.as_mut()) {
                        cb(st, &s);
                    }
                }
                if s.current != current || s.last_good != last_good {
                    current = s.current.clone();
                    last_good = s.last_good.clone();
                    if let Some(cb) = handlers.on_version_change.as_mut() {
                        cb(s.current.as_deref(), s.last_good.as_deref(), &s);
                    }
                }
                if s.hold != hold {
                    hold = s.hold;
                    if let Some(cb) = handlers.on_hold.as_mut() {
                        cb(s.hold, &s);
                    }
                }
                if s.last_error != last_error {
                    last_error = s.last_error.clone();
                    if let (Some(e), Some(cb)) =
                        (s.last_error.as_deref(), handlers.on_error.as_mut())
                    {
                        cb(e, &s);
                    }
                }
                if self.prepare_requested(&s) {
                    if !prompted {
                        prompted = true;
                        if let Some(cb) = handlers.on_prepare.as_mut() {
                            cb(&s);
                        }
                    }
                } else {
                    prompted = false;
                }
            }
            thread::sleep(interval);
        }
    }

    fn set_ready(&self, token: String) -> Result<()> {
        self.update(|o| {
            o.insert("ready".into(), Value::from(token));
        })?;
        Ok(())
    }

    fn require_instance(&self) -> Result<&str> {
        if self.instance.is_empty() {
            Err("lode: no LODE_INSTANCE — readiness needs a supervised launch".into())
        } else {
            Ok(&self.instance)
        }
    }

    fn temp_path(&self) -> PathBuf {
        self.lode_dir
            .join(format!("state.json.{}.tmp", std::process::id()))
    }
}

/// Callbacks for [`Lode::watch`] — lode's notifications. Each fires on change
/// only; any may be left `None` (build via `Handlers::default()`).
#[derive(Default)]
pub struct Handlers<'a> {
    /// `config_generation` rose (operator edited lode.toml); apply via [`Lode::reload_config`].
    pub on_config_change: Option<Box<dyn FnMut(u64, &State) + 'a>>,
    /// A newer version is advertised (`available`, under policy = check).
    pub on_available: Option<Box<dyn FnMut(&str, &State) + 'a>>,
    /// Lifecycle status changed.
    pub on_status: Option<Box<dyn FnMut(Status, &State) + 'a>>,
    /// `current`/`last_good` changed — an update committed or a rollback landed.
    pub on_version_change: Option<Box<dyn FnMut(Option<&str>, Option<&str>, &State) + 'a>>,
    /// The `hold` flag was set/cleared (a maintenance hold).
    pub on_hold: Option<Box<dyn FnMut(bool, &State) + 'a>>,
    /// lode recorded a (non-fatal) error.
    pub on_error: Option<Box<dyn FnMut(&str, &State) + 'a>>,
    /// Staged-update prepare prompt; drain then [`Lode::ack_prepared`].
    pub on_prepare: Option<Box<dyn FnMut(&State) + 'a>>,
    /// Every tick, the full snapshot.
    pub on_state: Option<Box<dyn FnMut(&State) + 'a>>,
}

/// RAII flock(2) guard for the RMW cycle; a failed open/lock degrades to lock-free.
struct LockGuard(Option<fs::File>);

impl LockGuard {
    fn acquire(path: &Path) -> Self {
        match fs::OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => {
                // SAFETY: a valid fd from an open File; flock takes (fd, op).
                unsafe { flock(file.as_raw_fd(), LOCK_EX) };
                LockGuard(Some(file))
            }
            Err(_) => LockGuard(None),
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            unsafe { flock(file.as_raw_fd(), LOCK_UN) };
        }
    }
}

/// True when this process is supervised by lode (`LODE_DIR` is set).
pub fn is_supervised() -> bool {
    std::env::var_os("LODE_DIR").is_some()
}

/// Your app's persistent data dir, resolved `DATA_DIR` > `LODE_DIR` > `ROOT_DIR`
/// (works with or without lode: set `ROOT_DIR` standalone, lode provides `LODE_DIR`,
/// or set `DATA_DIR` to override). `None` if none are set.
pub fn data_dir() -> Option<String> {
    ["DATA_DIR", "LODE_DIR", "ROOT_DIR"]
        .into_iter()
        .find_map(|k| std::env::var(k).ok().filter(|s| !s.is_empty()))
}

/// Your app's root/run dir convention (`ROOT_DIR`).
pub fn root_dir() -> Option<String> {
    std::env::var("ROOT_DIR").ok().filter(|s| !s.is_empty())
}

/// lode's own dir, where state.json lives (`LODE_DIR`).
pub fn lode_dir() -> Option<String> {
    std::env::var("LODE_DIR").ok().filter(|s| !s.is_empty())
}

/// lode's runtime dir for this app — its cwd (`LODE_WORKDIR`).
pub fn workdir() -> Option<String> {
    std::env::var("LODE_WORKDIR").ok().filter(|s| !s.is_empty())
}

/// The version lode launched (`LODE_ACTIVE_VERSION`).
pub fn active_version() -> Option<String> {
    std::env::var("LODE_ACTIVE_VERSION")
        .ok()
        .filter(|s| !s.is_empty())
}

/// This launch's unique id (`LODE_INSTANCE`, `{pid}-{nanoid}`).
pub fn instance_id() -> String {
    std::env::var("LODE_INSTANCE").unwrap_or_default()
}

/// The readiness mode in force (`"none"` | `"state"`).
pub fn readiness() -> Option<String> {
    std::env::var("LODE_READINESS")
        .ok()
        .filter(|s| !s.is_empty())
}

static TERMINATING: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_sig: c_int) {
    // Async-signal-safe: a single atomic store.
    TERMINATING.store(true, Ordering::SeqCst);
}

/// Install SIGTERM/SIGINT handlers that flip [`terminating`]; poll it from your
/// serve loop and drain + `exit(0)`.
pub fn install_term_handler() {
    unsafe {
        signal(SIGTERM, on_term);
        signal(SIGINT, on_term);
    }
}

/// Whether SIGTERM/SIGINT has arrived since [`install_term_handler`].
pub fn terminating() -> bool {
    TERMINATING.load(Ordering::SeqCst)
}
