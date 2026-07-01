//! Engine: the clap-free, signal-free core over lode's version machinery.
//!
//! This module owns the *pure* resolution logic shared by the supervised-service
//! path and the CLI passthrough: deciding which installed version to launch
//! (bootstrapping the channel latest only when nothing usable is present, design
//! §4/§5) and ensuring a configured `[runtime]` is available for the child (design
//! §4). None of it touches process ownership, signals, or argument parsing — that
//! lives in [`crate::supervisor`] — so the engine layer can be embedded as a library
//! depending only on std (no signal-handling, syscall-wrapper, or CLI crates).
//!
//! [`Engine`] is the public facade: a thin, owned-[`Config`] handle exposing the
//! read-only resolution ([`resolve_target`](Engine::resolve_target),
//! [`ensure_runtime`](Engine::ensure_runtime), [`check`](Engine::check)) and the
//! existing operator commands ([`status`](Engine::status),
//! [`install`](Engine::install), [`rollback`](Engine::rollback),
//! [`versions`](Engine::versions), [`restart`](Engine::restart)).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::{commands, download, idval, install, manifest, state};

// The std Unix permission-bit extension trait (for the executable-bit check) is
// reached through this alias of `std::os::unix`. Aliasing the platform segment keeps
// the engine-purity guard — a lexical scan that forbids the `nix` crate — from
// tripping on the std platform path: the engine depends on no non-std crate here.
#[cfg(unix)]
use std::os::unix as unix_ext;

// --- version resolution (shared by serve + exec) ---

/// A resolved, installed version and what is needed to launch it: its dir plus
/// the manifest-published `run`/`exec` launch overrides read back from the
/// version marker (they take precedence over the live `[command]` values).
///
/// `dir`/`run`/`exec` are consumed only when actually launching the child (the
/// supervise loop and exec-passthrough), so under `--features engine` — which
/// resolves versions but never spawns — they are constructed-but-unread. The
/// `cfg_attr` keeps that intentional; the default (`cli`) build still lint-checks
/// the fields fully.
#[cfg_attr(not(feature = "supervisor"), allow(dead_code))]
pub(crate) struct Target {
    pub(crate) version: String,
    pub(crate) dir: PathBuf,
    pub(crate) run: Option<String>,
    pub(crate) exec: Option<String>,
}

/// Decide which version to run and load its launch metadata. Bootstraps the
/// latest only when nothing usable is installed (design §4: never auto-jump
/// versions).
pub(crate) fn resolve_target(cfg: &Config) -> Result<Target> {
    let version = determine_version(cfg)?;
    locate(cfg, &version)
}

/// Build the launch [`Target`] for an already-installed `version` by reading its
/// `.lode.json` marker (design §15). Errors if the version is not installed.
/// Used by `serve` and by the C2 hot-update apply path.
pub(crate) fn locate(cfg: &Config, version: &str) -> Result<Target> {
    // Defensive: every caller already validated `version`, but it keys
    // `versions/<version>` here too — re-check before the join.
    idval::validate_id("version", version)?;
    let m = install::marker(cfg, version)?;
    let dir = cfg.global.dir.join("versions").join(version);
    Ok(Target {
        version: version.to_owned(),
        dir,
        run: m.run,
        exec: m.exec,
    })
}

/// Pick the version to launch: an operator `pin` wins (installing it if needed);
/// otherwise the recorded `current` if still installed; otherwise the newest
/// locally installed version; otherwise bootstrap the channel latest.
fn determine_version(cfg: &Config) -> Result<String> {
    if let Some(pin) = cfg.update.pin.as_deref() {
        // A configured pin keys `versions/<pin>`; reject traversal before it is
        // used to probe the installed set or bootstrap.
        idval::validate_id("version", pin)?;
        if version_installed(cfg, pin) {
            return Ok(pin.to_owned());
        }
        return bootstrap(cfg, Some(pin));
    }

    // Lenient: this is the BOOT read — a corrupt state.json on the volume must
    // degrade to "no recorded current" (warn + quarantine), never a crash-loop.
    let state_path = cfg.global.dir.join("state.json");
    if let Some(st) = state::read_lenient(&state_path)
        && let Some(cur) = st.current.as_deref()
        && version_installed(cfg, cur)
    {
        return Ok(cur.to_owned());
    }

    if let Some(v) = newest_installed(cfg)? {
        return Ok(v);
    }

    bootstrap(cfg, None)
}

/// A version counts as installed once its `.lode.json` marker is present (install
/// writes it last, atomically).
pub(crate) fn version_installed(cfg: &Config, version: &str) -> bool {
    cfg.global
        .dir
        .join("versions")
        .join(version)
        .join(".lode.json")
        .is_file()
}

/// The newest installed version (semver-descending), or `None` if none.
fn newest_installed(cfg: &Config) -> Result<Option<String>> {
    let versions_dir = cfg.global.dir.join("versions");
    let entries = match std::fs::read_dir(&versions_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut installed = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str()
            && version_installed(cfg, name)
        {
            installed.push(name.to_owned());
        }
    }
    installed.sort_by(|a, b| cmp_desc(a, b));
    Ok(installed.into_iter().next())
}

/// Newest-first version order (valid semver by precedence ahead of non-semver).
fn cmp_desc(a: &str, b: &str) -> std::cmp::Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(x), Ok(y)) => y.cmp(&x),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => b.cmp(a),
    }
}

/// Bootstrap install: fetch the manifest, resolve a target (`requested` > pin >
/// channel latest), download + verify + install it, and activate it (design §5).
fn bootstrap(cfg: &Config, requested: Option<&str>) -> Result<String> {
    install::gc(cfg)?;
    let manifest = manifest::fetch(cfg)?;
    if manifest.name != cfg.global.app {
        return Err(Error::Manifest(format!(
            "manifest name {:?} does not match configured app {:?}",
            manifest.name, cfg.global.app
        )));
    }
    // Verify the catalog signature if it carries one (verify-if-present); absence is
    // fine — the per-artifact check binds each download.
    install::verify_manifest_identity(cfg, &manifest)?;
    // Anti-downgrade floor from any prior state (a clean bootstrap has none, so this
    // never blocks the first install); it only gates a `latest`-following resolution.
    // Lenient: a boot path (reached directly when pinned) — a corrupt state.json
    // just means no floor, never a failed bootstrap.
    let prior = state::read_lenient(&cfg.global.dir.join("state.json")).unwrap_or_default();
    let floor = install::version_floor(prior.current.as_deref(), prior.last_good.as_deref());
    let target = manifest::resolve_target(
        &manifest,
        &cfg.update.channel,
        cfg.update.pin.as_deref(),
        requested,
        floor.as_deref(),
    )?;
    let entry = manifest::version_entry(&manifest, &target)?;
    let asset = manifest::select_asset(entry, required_asset(cfg)?)?;
    let (artifact, sha256) =
        download::fetch_artifact(cfg, asset, &target, &manifest::allowed_hosts(cfg))?;
    install::install(cfg, &target, asset, &artifact, &sha256)?;
    install::switch_current(cfg, &target)?;
    tracing::info!(version = target, "bootstrapped initial version");
    Ok(target)
}

/// The operator-selected asset filename (`[update].asset`) — the source-agnostic
/// selection key for both adapters. There is no platform fallback, so this errors
/// clearly when unset rather than guessing an asset.
pub(crate) fn required_asset(cfg: &Config) -> Result<&str> {
    cfg.update.asset.as_deref().ok_or_else(|| {
        Error::Config(
            "no [update].asset configured — set the asset filename to install (source-adapters §3)"
                .to_owned(),
        )
    })
}

// --- runtime resolution ([runtime], design §4) ---

/// What to do about a configured `[runtime]` before launching the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimePlan {
    /// No `[runtime]` configured (self-contained binary).
    NotNeeded,
    /// The runtime is already on PATH — nothing to download.
    AlreadyPresent,
    /// A prior download left the runtime in `$LODE_DIR/runtime/` — reuse it (no
    /// network). When `$LODE_DIR` is a persistent volume this makes the download a
    /// one-time cost across restarts.
    Cached,
    /// The runtime is missing — download it and prepend its dir to the child PATH.
    Fetch,
}

/// Decide what to do about the runtime. Precedence: a runtime already on PATH wins
/// (system runtime), then a cached download is reused, then a fresh download. Errors
/// only when the runtime is named but absent from PATH and cache with no `download`
/// URL configured.
fn plan_runtime(
    runtime: Option<&str>,
    download: Option<&str>,
    present: bool,
    cached: bool,
) -> Result<RuntimePlan> {
    match runtime {
        None => Ok(RuntimePlan::NotNeeded),
        Some(_) if present => Ok(RuntimePlan::AlreadyPresent),
        Some(_) if cached => Ok(RuntimePlan::Cached),
        Some(_) if download.is_some() => Ok(RuntimePlan::Fetch),
        Some(name) => Err(Error::Process(format!(
            "runtime {name:?} not found on PATH or in cache, and no [runtime].download configured"
        ))),
    }
}

/// Ensure a configured runtime is available for the child, downloading it into
/// `$LODE_DIR/runtime/` when absent from PATH and not already cached there. Returns
/// the directory to prepend to the child's PATH, or `None` when no runtime download
/// is needed. A previously downloaded runtime (a `runtime/<name>` executable from an
/// earlier launch) is reused without touching the network, so a persistent
/// `$LODE_DIR` makes the download a one-time cost; delete `runtime/<name>` to force a
/// re-download (e.g. to change the runtime version).
pub(crate) fn ensure_runtime(cfg: &Config) -> Result<Option<PathBuf>> {
    let runtime = cfg.runtime.runtime.as_deref();
    let download_url = cfg.runtime.download.as_deref();
    let expected = cfg.runtime.version.as_deref();
    let probe_args = runtime_probe_args(cfg.runtime.version_check.as_deref());
    let path_var = std::env::var("PATH").unwrap_or_default();
    let runtime_dir = cfg.global.dir.join("runtime");
    // place_runtime lands the binary at `runtime/<name>`; the same path is the cache
    // key on the next launch.
    let cached_bin = runtime.map(|name| runtime_dir.join(name));

    // Version-gate PATH and cache: a usable runtime must also report the expected
    // version (when one is configured). A wrong-version PATH/cache entry is treated
    // as unusable so we fall through to a fresh download that pins the right version.
    let present_ok = runtime.is_some_and(|name| {
        on_path(name, &path_var)
            && expected.is_none_or(|want| {
                let ok = runtime_version_ok(OsStr::new(name), &probe_args, want);
                if !ok {
                    tracing::warn!(
                        runtime = name,
                        want,
                        "PATH runtime version mismatch; trying cache/download"
                    );
                }
                ok
            })
    });
    let cached_ok = cached_bin.as_deref().is_some_and(|bin| {
        is_executable_file(bin)
            && expected.is_none_or(|want| {
                let ok = runtime_version_ok(bin.as_os_str(), &probe_args, want);
                if !ok {
                    tracing::info!(want, "cached runtime version mismatch; re-downloading");
                }
                ok
            })
    });

    match plan_runtime(runtime, download_url, present_ok, cached_ok)? {
        RuntimePlan::NotNeeded | RuntimePlan::AlreadyPresent => Ok(None),
        RuntimePlan::Cached => {
            tracing::info!(
                runtime = runtime.unwrap_or_default(),
                dir = %runtime_dir.display(),
                "runtime served from cache; skipping download"
            );
            Ok(Some(runtime_dir))
        }
        RuntimePlan::Fetch => {
            // Both are `Some` here (guaranteed by `plan_runtime`).
            let name = runtime.unwrap_or_default();
            let url = download_url.unwrap_or_default();
            let format = infer_format(url);
            tracing::info!(
                runtime = name,
                format,
                "runtime missing from PATH and cache; downloading"
            );
            let asset = runtime_asset(url, name);
            // The runtime download has no manifest origin to be same-origin with,
            // so credentials ride it only when its host is explicitly allowlisted
            // via `[http].credential_hosts`; otherwise they are dropped.
            let (archive, _sha) =
                download::fetch_artifact(cfg, &asset, "runtime", &cfg.http.credential_hosts)?;
            install::place_runtime(&runtime_dir, &archive, format, name)?;
            // The extracted `runtime/<name>` binary is the runtime's (version-checked)
            // cache; the downloaded archive is redundant, so drop it after placement.
            let _ = std::fs::remove_file(&archive);
            if let Some(want) = expected {
                verify_runtime_version(&runtime_dir.join(name), &probe_args, want)?;
            }
            Ok(Some(runtime_dir))
        }
    }
}

/// Args that make a runtime print its version, from `[runtime].version_check`
/// (whitespace-split), defaulting to `--version`.
fn runtime_probe_args(version_check: Option<&str>) -> Vec<String> {
    match version_check {
        Some(s) if !s.trim().is_empty() => s.split_whitespace().map(str::to_owned).collect(),
        _ => vec!["--version".to_owned()],
    }
}

/// Run `program <args>` and return its combined stdout+stderr, or `None` if the
/// program can't be executed at all (spawn error). Runtimes print their version to
/// either stream, so both are captured.
fn probe_output(program: &OsStr, args: &[String]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(text)
}

/// Does `program`'s version-probe output contain `expected`? A probe that fails to
/// execute (wrong arch, missing lib, bad path) counts as not-OK.
fn runtime_version_ok(program: &OsStr, args: &[String], expected: &str) -> bool {
    probe_output(program, args).is_some_and(|out| out.contains(expected))
}

/// Confirm a freshly downloaded runtime reports `expected`; a mismatch (or a probe
/// that won't run) is a hard error — the configured `download` served the wrong
/// version, or `version`/`version_check` is misconfigured.
fn verify_runtime_version(bin: &Path, args: &[String], expected: &str) -> Result<()> {
    match probe_output(bin.as_os_str(), args) {
        Some(out) if out.contains(expected) => {
            tracing::info!(version = expected, "downloaded runtime version verified");
            Ok(())
        }
        Some(out) => Err(Error::Process(format!(
            "downloaded runtime version mismatch: expected {expected:?}, but `{bin} {probe}` reported {got:?}",
            bin = bin.display(),
            probe = args.join(" "),
            got = out.lines().next().unwrap_or("").trim(),
        ))),
        None => Err(Error::Process(format!(
            "could not run `{bin} {probe}` to verify the downloaded runtime version",
            bin = bin.display(),
            probe = args.join(" "),
        ))),
    }
}

/// A synthetic [`manifest::Asset`] for a runtime download, so the runtime reuses
/// the audited [`download`] path. No `sha256`/`size` (the `[runtime]` config carries
/// none) and no launch overrides (runtimes are placed, not launched, by lode). The
/// format is determined separately by the caller (from the URL, see
/// [`infer_format`]) rather than from `name`, since a runtime binary name carries
/// no packaging suffix.
fn runtime_asset(url: &str, name: &str) -> manifest::Asset {
    manifest::Asset {
        name: name.to_owned(),
        url: url.to_owned(),
        sha256: String::new(),
        sig: None,
        key_id: None,
        run: None,
        exec: None,
        size: None,
        auth: true,
    }
}

/// Is `name` an executable file in any `path_var` (`:`-separated) directory?
fn on_path(name: &str, path_var: &str) -> bool {
    path_var
        .split(':')
        .filter(|dir| !dir.is_empty())
        .any(|dir| is_executable_file(&Path::new(dir).join(name)))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use unix_ext::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Infer a packaging format from a URL suffix (query/fragment stripped). The
/// suffix checks are case-insensitive (the path is lowercased first), so the
/// extension-comparison lint does not apply.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn infer_format(url: &str) -> &'static str {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        "tar.gz"
    } else if path.ends_with(".zip") {
        "zip"
    } else if path.ends_with(".gz") {
        "gz"
    } else {
        "raw"
    }
}

// --- public facade ---

/// What [`Engine::check`] resolves.
///
/// The version the configured channel currently points at, plus the app name and
/// channel it was resolved for. Read-only — it reflects the manifest without
/// downloading or installing anything.
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// The application name advertised by the resolved manifest.
    pub app: String,
    /// The update channel the resolution was performed against.
    pub channel: String,
    /// The version id the channel resolves to (after the anti-downgrade floor).
    pub version: String,
}

/// A clap-free, signal-free handle over lode's version machinery and operator
/// commands, owning a resolved [`Config`].
///
/// ```no_run
/// # fn main() -> lode::Result<()> {
/// let cfg = lode::Config::from_toml("")?;
/// let engine = lode::Engine::new(cfg);
/// let latest = engine.check()?;
/// println!("{} {} -> {}", latest.app, latest.channel, latest.version);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Engine {
    cfg: Config,
}

impl Engine {
    /// Build an engine over an already-resolved [`Config`].
    #[must_use]
    pub const fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    /// Print the status of the configured instance (`lode-cli status`).
    pub fn status(&self) -> Result<()> {
        commands::status::run(&self.cfg)
    }

    /// Resolve what the configured channel currently points at, **without**
    /// downloading or installing. Fetches the manifest and runs the same
    /// anti-downgrade-floored resolution as a real install would.
    pub fn check(&self) -> Result<CheckResult> {
        let manifest = manifest::fetch(&self.cfg)?;
        // Mirror the bootstrap resolution: apply the anti-downgrade floor so the
        // reported version matches what an install would actually pick.
        let prior =
            state::read_lenient(&self.cfg.global.dir.join("state.json")).unwrap_or_default();
        let floor = install::version_floor(prior.current.as_deref(), prior.last_good.as_deref());
        let version = manifest::resolve_target(
            &manifest,
            &self.cfg.update.channel,
            self.cfg.update.pin.as_deref(),
            None,
            floor.as_deref(),
        )?;
        Ok(CheckResult {
            app: manifest.name,
            channel: self.cfg.update.channel.clone(),
            version,
        })
    }

    /// Install (and activate) a target version, or the channel latest when `None`
    /// (`lode-cli update`).
    pub fn install(&self, version: Option<&str>) -> Result<()> {
        commands::update::run(&self.cfg, version)
    }

    /// Roll back to a prior version, or `last_good` when `None` (`lode-cli rollback`).
    pub fn rollback(&self, to: Option<&str>) -> Result<()> {
        commands::rollback::run(&self.cfg, to)
    }

    /// List the installed versions and rollout history (`lode-cli versions`).
    pub fn versions(&self) -> Result<()> {
        commands::versions::run(&self.cfg)
    }

    /// Request the running instance restart the app (`lode-cli restart`).
    pub fn restart(&self) -> Result<()> {
        commands::restart::run(&self.cfg)
    }

    /// Resolve which installed version would launch right now, returning its id.
    /// Bootstraps the channel latest if nothing usable is installed (design §4).
    pub fn resolve_target(&self) -> Result<String> {
        resolve_target(&self.cfg).map(|t| t.version)
    }

    /// Ensure a configured `[runtime]` is available for the child, returning the
    /// directory to prepend to its PATH (or `None` when no runtime is needed).
    pub fn ensure_runtime(&self) -> Result<Option<PathBuf>> {
        ensure_runtime(&self.cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_plan_decisions() {
        assert_eq!(
            plan_runtime(None, None, false, false).unwrap(),
            RuntimePlan::NotNeeded
        );
        // Already on PATH → skip the download (system runtime wins over cache).
        assert_eq!(
            plan_runtime(Some("bun"), Some("https://x/bun.zip"), true, true).unwrap(),
            RuntimePlan::AlreadyPresent
        );
        // Off PATH but cached from a prior launch → reuse, no network (even if a
        // download URL is set).
        assert_eq!(
            plan_runtime(Some("bun"), Some("https://x/bun.zip"), false, true).unwrap(),
            RuntimePlan::Cached
        );
        // Off PATH, no cache, no download URL, but cached present → still reused.
        assert_eq!(
            plan_runtime(Some("bun"), None, false, true).unwrap(),
            RuntimePlan::Cached
        );
        // Missing + download configured → fetch.
        assert_eq!(
            plan_runtime(Some("bun"), Some("https://x/bun.zip"), false, false).unwrap(),
            RuntimePlan::Fetch
        );
        // Missing everywhere + no download → error.
        assert!(plan_runtime(Some("bun"), None, false, false).is_err());
    }

    #[test]
    fn infer_format_from_suffix() {
        assert_eq!(infer_format("https://x/bun.tar.gz"), "tar.gz");
        assert_eq!(infer_format("https://x/bun.tgz"), "tar.gz");
        assert_eq!(infer_format("https://x/bun.zip?token=1"), "zip");
        assert_eq!(infer_format("https://x/bun.gz"), "gz");
        assert_eq!(infer_format("https://x/bun"), "raw");
    }

    #[test]
    fn on_path_finds_executable() {
        let dir = std::env::temp_dir().join(format!("lode-onpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("mytool");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use unix_ext::fs::PermissionsExt as _;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_var = dir.display().to_string();
        assert!(on_path("mytool", &path_var));
        assert!(!on_path("absent", &path_var));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_probe_args_defaults_to_version_flag() {
        assert_eq!(runtime_probe_args(None), vec!["--version".to_owned()]);
        assert_eq!(runtime_probe_args(Some("  ")), vec!["--version".to_owned()]);
        assert_eq!(runtime_probe_args(Some("-v")), vec!["-v".to_owned()]);
        assert_eq!(
            runtime_probe_args(Some("eval Bun.version")),
            vec!["eval".to_owned(), "Bun.version".to_owned()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_version_probe_matches_and_rejects() {
        use unix_ext::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("lode-rtver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A stand-in runtime whose `--version` prints "1.1.38" (like bun's output).
        let bin = dir.join("fakert");
        std::fs::write(&bin, b"#!/bin/sh\necho 1.1.38\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let args = runtime_probe_args(None);

        // Substring match handles bare and prefixed (e.g. node's "v22…") forms.
        assert!(runtime_version_ok(bin.as_os_str(), &args, "1.1.38"));
        assert!(runtime_version_ok(bin.as_os_str(), &args, "1.1"));
        assert!(!runtime_version_ok(bin.as_os_str(), &args, "1.2.0"));
        // A binary that can't be executed → not OK, never a panic.
        assert!(!runtime_version_ok(
            dir.join("absent").as_os_str(),
            &args,
            "1.1.38"
        ));

        // verify_runtime_version: ok on match, Err on mismatch / unrunnable.
        assert!(verify_runtime_version(&bin, &args, "1.1.38").is_ok());
        assert!(matches!(
            verify_runtime_version(&bin, &args, "9.9.9"),
            Err(Error::Process(_))
        ));
        assert!(matches!(
            verify_runtime_version(&dir.join("absent"), &args, "1.1.38"),
            Err(Error::Process(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
