//! Supervised-service runtime + CLI passthrough (design §5/§8/§9).
//!
//! [`serve`] is the bare-`lode` path: acquire the single-instance lock, clean up
//! orphans/garbage from a previous run, decide which version to launch (bootstrap
//! the latest only when nothing is installed), then spawn the app as a child and
//! supervise it. By default (`supervise.restart=on-failure`) lode keeps the app
//! alive: a failing app is retried with exponential backoff and, after the retry
//! cap, lode *pauses* — it stays alive (never crash-loops the container / exits as
//! PID 1) until a recovery trigger (an edited `lode.toml`, a bumped `restart_nonce`,
//! or a new `target`). `off` opts back into mirror-the-child (lode exits with it); a
//! clean `exit(0)` exits lode (use `always` to retry that too). lode also relaunches
//! the child for lode-initiated transitions — an update, a single-strike rollback,
//! or an explicit restart — does signal forwarding, graceful stop, and (as
//! PID 1) child-subreaping so re-parented grandchildren never become zombies. On
//! the same short-interval tick the loop also drives the C2 update
//! machinery: it polls `state.json`'s mtime for app-written `target` /
//! `restart_nonce` requests (§7), runs the `[update].policy` check (§5), and — when
//! a target is applied — performs the stop-start hot-update with the readiness/stop
//! handshake (§8) and automatic rollback to `last_good` on failure.
//!
//! [`exec_passthrough`] is the `lode <args>` path: validate the version (bootstrap
//! if none), prepare the same argv/env/runtime, then `exec`-replace into the app —
//! no lock, no supervision, no polling. The replacement uses the safe
//! [`std::os::unix::process::CommandExt::exec`], so the crate keeps
//! `#![forbid(unsafe_code)]`.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::ffi::OsStr;
use std::os::raw::c_int;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use signal_hook::iterator::Signals;

use crate::cli::Globals;
use crate::config::{self, Config, Policy, Readiness, RestartPolicy};
use crate::error::{Error, Result};
use crate::state::{self, HistoryEntry, HistoryResult, State, Status};
use crate::{download, idval, install, manifest};

/// Supervise-loop tick. Bounds signal-forwarding and child-exit latency while
/// leaving headroom for the C2 state-poll / update-observation on the same cadence.
const POLL_TICK: Duration = Duration::from_millis(200);

/// Poll granularity while waiting for a child to exit during a graceful stop.
const STOP_POLL: Duration = Duration::from_millis(50);

/// How often the supervisor re-checks `state.json`'s mtime for app-written
/// `target` / `restart_nonce` requests (design §7: notification via mtime poll).
const STATE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Cap on the persisted rollout `history` so `state.json` cannot grow unbounded.
const HISTORY_CAP: usize = 20;

// --- public entry points ---

/// What ended a supervise run: exit lode, or reload `lode.toml` and re-attempt.
enum Outcome {
    /// Stop lode with this exit code (graceful shutdown, or mirror exit under `off`).
    Exit(ExitCode),
    /// `lode.toml` changed while paused — re-resolve config and re-run (design §8).
    Reload,
}

/// Run the app as a supervised service (bare `lode`). Holds the single-instance
/// lock across config reloads: each run supervises the app until graceful shutdown
/// (→ return) or a `lode.toml`-change recovery (→ re-resolve config and re-run).
/// Returns the child's exit code on shutdown.
pub(crate) fn serve(globals: &Globals) -> Result<ExitCode> {
    set_subreaper();
    let mut cfg = config::resolve(globals)?;
    let _lock = lock_acquire(&cfg)?;
    // Install the signal handlers ONCE, before any bootstrap work: resolve/install
    // and the runtime fetch may download for minutes, and as PID 1 an unhandled
    // SIGTERM is simply ignored — `docker stop` would hang until the SIGKILL.
    let mut signals = Signals::new(registration_set(
        &forward_signals(&cfg.signals.forward),
        cfg.signals.restart.as_deref().and_then(parse_signal),
    ))
    .map_err(|e| Error::Process(format!("install signal handlers: {e}")))?;
    loop {
        startup_cleanup(&cfg)?;
        if let Some(code) = bootstrap_terminated(&mut signals, &cfg) {
            return Ok(code);
        }
        let target = resolve_target(&cfg)?;
        install::switch_current(&cfg, &target.version)?;
        if let Some(code) = bootstrap_terminated(&mut signals, &cfg) {
            return Ok(code);
        }
        let runtime_dir = ensure_runtime(&cfg)?;
        if let Some(code) = bootstrap_terminated(&mut signals, &cfg) {
            return Ok(code);
        }

        let mut supervisor = Supervisor::new(&cfg, target, runtime_dir);
        match supervisor.run(&mut signals)? {
            Outcome::Exit(code) => return Ok(code),
            // A paused app whose `lode.toml` was edited: re-resolve and re-attempt.
            // An invalid edit keeps the previous config (lode stays alive, awaits
            // another edit) rather than taking down PID 1.
            Outcome::Reload => match config::resolve(globals) {
                Ok(next) => {
                    tracing::info!("lode.toml changed; reloaded config, re-attempting app");
                    cfg = next;
                }
                Err(e) => {
                    tracing::error!(error = %e, "reload: invalid lode.toml; keeping previous config");
                }
            },
        }
    }
}

/// CLI passthrough (`lode <args>`): validate the version, then `exec`-replace into
/// `[command].exec` + `args`. On success it never returns (the process image is
/// replaced); any failure surfaces as [`Error::Process`].
pub(crate) fn exec_passthrough(cfg: &Config, args: &[String]) -> Result<Infallible> {
    let target = resolve_target(cfg)?;
    let runtime_dir = ensure_runtime(cfg)?;
    let instance = format!("{}-exec", std::process::id());

    let dir = target.dir.to_string_lossy();
    let exec = effective_command(target.exec.as_deref(), cfg.command.exec.as_deref(), "exec")?;
    let command_line = build_exec_argv(&exec, &dir, args)?;
    let env = child_env(
        std::env::vars(),
        &cfg.env,
        &target.version,
        &cfg.global.data_dir,
        &instance,
        runtime_dir.as_deref(),
    );
    let workdir = PathBuf::from(expand_token(&cfg.command.workdir, &dir));

    let (program, rest) = command_line
        .split_first()
        .ok_or_else(|| Error::Process("empty exec command".to_owned()))?;
    let mut cmd = Command::new(program);
    cmd.args(rest).current_dir(&workdir).env_clear();
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    // `exec` only returns on failure; on success this process is replaced.
    let err = cmd.exec();
    Err(Error::Process(format!("exec {program}: {err}")))
}

// --- version resolution (shared by serve + exec) ---

/// A resolved, installed version and what is needed to launch it: its dir plus
/// the manifest-published `run`/`exec` launch overrides read back from the
/// version marker (they take precedence over the live `[command]` values).
struct Target {
    version: String,
    dir: PathBuf,
    run: Option<String>,
    exec: Option<String>,
}

/// Decide which version to run and load its launch metadata. Bootstraps the
/// latest only when nothing usable is installed (design §4: never auto-jump
/// versions).
fn resolve_target(cfg: &Config) -> Result<Target> {
    let version = determine_version(cfg)?;
    locate(cfg, &version)
}

/// Build the launch [`Target`] for an already-installed `version` by reading its
/// `.lode.json` marker (design §15). Errors if the version is not installed.
/// Used by `serve` and by the C2 hot-update apply path.
fn locate(cfg: &Config, version: &str) -> Result<Target> {
    // Defensive: every caller already validated `version`, but it keys
    // `versions/<version>` here too — re-check before the join.
    idval::validate_id("version", version)?;
    let m = install::marker(cfg, version)?;
    let dir = cfg.global.data_dir.join("versions").join(version);
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
    let state_path = cfg.global.data_dir.join("state.json");
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
fn version_installed(cfg: &Config, version: &str) -> bool {
    cfg.global
        .data_dir
        .join("versions")
        .join(version)
        .join(".lode.json")
        .is_file()
}

/// The newest installed version (semver-descending), or `None` if none.
fn newest_installed(cfg: &Config) -> Result<Option<String>> {
    let versions_dir = cfg.global.data_dir.join("versions");
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
    let prior = state::read_lenient(&cfg.global.data_dir.join("state.json")).unwrap_or_default();
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
fn required_asset(cfg: &Config) -> Result<&str> {
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
    /// A prior download left the runtime in `$DATA_DIR/runtime/` — reuse it (no
    /// network). When `$DATA_DIR` is a persistent volume this makes the download a
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
/// `$DATA_DIR/runtime/` when absent from PATH and not already cached there. Returns
/// the directory to prepend to the child's PATH, or `None` when no runtime download
/// is needed. A previously downloaded runtime (a `runtime/<name>` executable from an
/// earlier launch) is reused without touching the network, so a persistent
/// `$DATA_DIR` makes the download a one-time cost; delete `runtime/<name>` to force a
/// re-download (e.g. to change the runtime version).
fn ensure_runtime(cfg: &Config) -> Result<Option<PathBuf>> {
    let runtime = cfg.runtime.runtime.as_deref();
    let download_url = cfg.runtime.download.as_deref();
    let expected = cfg.runtime.version.as_deref();
    let probe_args = runtime_probe_args(cfg.runtime.version_check.as_deref());
    let path_var = std::env::var("PATH").unwrap_or_default();
    let runtime_dir = cfg.global.data_dir.join("runtime");
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
    use std::os::unix::fs::PermissionsExt as _;
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

// --- argv + environment ---

/// The launch command actually in force: the manifest asset's signed override
/// (from the version marker) wins over the operator's `[command]` value; blank
/// strings count as unset on both sides. When neither supplies one, launching is
/// impossible — a clear, actionable hard error (design: entry abolition).
fn effective_command(
    override_cmd: Option<&str>,
    configured: Option<&str>,
    kind: &str,
) -> Result<String> {
    override_cmd
        .filter(|c| !c.trim().is_empty())
        .or_else(|| configured.filter(|c| !c.trim().is_empty()))
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::Process(format!(
                "no {kind} command: set [command].{kind} or publish `{kind}` in the manifest asset"
            ))
        })
}

/// Expand the `{dir}` placeholder (the running version dir) in one token. The
/// braces are a literal placeholder to substitute, not Rust format args.
#[allow(clippy::literal_string_with_formatting_args)]
fn expand_token(token: &str, dir: &str) -> String {
    token.replace("{dir}", dir)
}

/// Absolutize a relative program path against the version dir: when the argv's
/// first token is a relative path naming an existing file in `dir` (e.g.
/// `./myapp`, the install-time chmod target), replace it with the absolute path —
/// launch then works regardless of an operator-overridden `workdir`. Any other
/// token (a PATH command like `bun`, or an absolute path) is left untouched.
fn resolve_program(argv: &mut [String], dir: &str) {
    if let Some(first) = argv.first_mut() {
        let path = Path::new(first.as_str());
        if path.is_relative() {
            // Drop a leading `./` so the joined path is clean in logs/argv[0].
            let rel = path.strip_prefix(".").unwrap_or(path);
            let candidate = Path::new(dir).join(rel);
            if candidate.is_file() {
                *first = candidate.to_string_lossy().into_owned();
            }
        }
    }
}

/// Build the bare-run argv from the effective run command: split on whitespace
/// (the command is LITERAL — no shell), expand `{dir}`, and absolutize a
/// version-dir-relative program. Never empty.
fn build_run_argv(command: &str, dir: &str) -> Result<Vec<String>> {
    let mut argv: Vec<String> = command
        .split_whitespace()
        .map(|t| expand_token(t, dir))
        .collect();
    if argv.is_empty() {
        return Err(Error::Process("empty run command".to_owned()));
    }
    resolve_program(&mut argv, dir);
    Ok(argv)
}

/// Build the passthrough argv from the effective exec command + `args`: split on
/// whitespace, expand `{dir}`, absolutize a version-dir-relative program, then
/// append the user args verbatim.
fn build_exec_argv(command: &str, dir: &str, args: &[String]) -> Result<Vec<String>> {
    let mut parts: Vec<String> = command
        .split_whitespace()
        .map(|t| expand_token(t, dir))
        .collect();
    if parts.is_empty() {
        return Err(Error::Process("empty exec command".to_owned()));
    }
    resolve_program(&mut parts, dir);
    parts.extend(args.iter().cloned());
    Ok(parts)
}

/// Build the child environment: inherit the host env minus all config `LODE_*`
/// vars, apply the operator's `[env]` overrides, optionally prepend the runtime dir
/// to PATH, then inject the read-only introspection vars (design §10). Precedence
/// (low → high): operator `[env]` defaults < inherited host env < runtime
/// PATH-prepend < lode's `LODE_*` vars.
fn child_env(
    host: impl IntoIterator<Item = (String, String)>,
    defined: &BTreeMap<String, String>,
    version: &str,
    data_dir: &Path,
    instance: &str,
    runtime_dir: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = host
        .into_iter()
        .filter(|(key, _)| !key.starts_with("LODE_"))
        .collect();
    // The `[env]` table is DEFAULTS: applied only for keys the inherited host env
    // doesn't already provide, so a per-deploy `-e KEY=…` (any inherited env var)
    // overrides `[env]`.
    for (key, value) in defined {
        if !env.iter().any(|(k, _)| k == key) {
            env.push((key.to_owned(), value.to_owned()));
        }
    }
    if let Some(dir) = runtime_dir {
        prepend_path(&mut env, dir);
    }
    // lode's introspection vars always win — set (not push) so a `[env]` entry of
    // the same name can't leave a duplicate behind.
    set_env(&mut env, "LODE_ACTIVE_VERSION", version);
    set_env(&mut env, "LODE_DATA_DIR", &data_dir.display().to_string());
    set_env(&mut env, "LODE_INSTANCE", instance);
    env
}

/// Set `key` to `value` in `env`, replacing an existing entry or appending a new
/// one (so the result never holds a duplicate key).
fn set_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, slot)) = env.iter_mut().find(|(k, _)| k == key) {
        value.clone_into(slot);
    } else {
        env.push((key.to_owned(), value.to_owned()));
    }
}

/// Prepend `dir` to the PATH entry in `env` (or create PATH if absent).
fn prepend_path(env: &mut Vec<(String, String)>, dir: &Path) {
    let dir = dir.display().to_string();
    if let Some((_, value)) = env.iter_mut().find(|(key, _)| key == "PATH") {
        *value = format!("{dir}:{value}");
    } else {
        env.push(("PATH".to_owned(), dir));
    }
}

/// The `LODE_READINESS` value injected for the child so it knows whether the
/// `state.ready` handshake is expected (design §8).
const fn readiness_label(mode: Readiness) -> &'static str {
    match mode {
        Readiness::None => "none",
        Readiness::State => "state",
    }
}

// --- signals ---

/// What an incoming signal means for the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Graceful shutdown: stop the child, release the lock, exit with its code.
    Terminate,
    /// Graceful restart: stop and re-spawn the child.
    Restart,
    /// Forward verbatim to the child.
    Forward,
    /// No supervisor action.
    Ignore,
}

/// Map a received signal to a supervisor action. The configured restart signal
/// wins (and is never forwarded); the termination set is handled next; remaining
/// members of the forward set are forwarded; everything else is ignored.
fn classify(sig: Signal, restart: Option<Signal>, forward: &[Signal]) -> Action {
    if restart == Some(sig) {
        Action::Restart
    } else if matches!(sig, Signal::SIGTERM | Signal::SIGINT | Signal::SIGQUIT) {
        Action::Terminate
    } else if forward.contains(&sig) {
        Action::Forward
    } else {
        Action::Ignore
    }
}

/// lode's standard forward set when `[signals].forward` is unset (design §8).
fn default_forward() -> Vec<Signal> {
    vec![
        Signal::SIGHUP,
        Signal::SIGUSR1,
        Signal::SIGUSR2,
        Signal::SIGWINCH,
        Signal::SIGCONT,
        Signal::SIGTSTP,
    ]
}

/// Resolve the configured forward set (or the standard set when empty), dropping
/// unparsable names with a warning.
fn forward_signals(configured: &[String]) -> Vec<Signal> {
    if configured.is_empty() {
        return default_forward();
    }
    configured
        .iter()
        .filter_map(|name| {
            let parsed = parse_signal(name);
            if parsed.is_none() {
                tracing::warn!(signal = name.as_str(), "ignoring unknown forward signal");
            }
            parsed
        })
        .collect()
}

/// Parse a signal name, accepting both `SIGHUP` and `HUP` (any case).
fn parse_signal(name: &str) -> Option<Signal> {
    let upper = name.trim().to_ascii_uppercase();
    let canonical = if upper.starts_with("SIG") {
        upper
    } else {
        format!("SIG{upper}")
    };
    canonical.parse().ok()
}

/// signal-hook refuses to register these (they cannot be caught or trigger UB).
const fn is_forbidden(sig: Signal) -> bool {
    matches!(
        sig,
        Signal::SIGKILL | Signal::SIGSTOP | Signal::SIGILL | Signal::SIGFPE | Signal::SIGSEGV
    )
}

/// The signals to register: termination set + forward set + restart signal,
/// minus the forbidden ones, deduplicated. A free function so [`serve`] can
/// install the set before any [`Supervisor`] exists (bootstrap must stay
/// interruptible) and [`Supervisor::run`] can re-add a reloaded config's set.
fn registration_set(forward: &[Signal], restart: Option<Signal>) -> Vec<c_int> {
    let mut wanted = vec![Signal::SIGTERM, Signal::SIGINT, Signal::SIGQUIT];
    wanted.extend(forward.iter().copied());
    if let Some(sig) = restart {
        wanted.push(sig);
    }
    let mut ints: Vec<c_int> = Vec::new();
    for sig in wanted {
        if is_forbidden(sig) {
            tracing::warn!(
                signal = sig.as_str(),
                "refusing to register forbidden signal"
            );
            continue;
        }
        let raw = sig as c_int;
        if !ints.contains(&raw) {
            ints.push(raw);
        }
    }
    ints
}

/// Act on a termination signal that arrived during bootstrap (between the
/// cleanup / resolve / runtime-download steps, before any child exists): write
/// the terminal `stopped` status best-effort and report the graceful exit code.
/// `None` => no termination pending, carry on. Restart/forward signals are
/// dropped here — there is no child yet to cycle or forward to.
fn bootstrap_terminated(signals: &mut Signals, cfg: &Config) -> Option<ExitCode> {
    let terminated = signals.pending().any(|raw| {
        Signal::try_from(raw)
            .is_ok_and(|sig| matches!(sig, Signal::SIGTERM | Signal::SIGINT | Signal::SIGQUIT))
    });
    if !terminated {
        return None;
    }
    tracing::info!("termination signal received during startup; exiting");
    // Best-effort (lenient read, swallowed write error): never block the
    // graceful exit on state.json (design §8).
    let path = cfg.global.data_dir.join("state.json");
    let mut st = state::read_lenient(&path).unwrap_or_default();
    st.status = Some(Status::Stopped);
    st.pid = None;
    if let Err(e) = state::write(&path, &st) {
        tracing::warn!(error = %e, "state.json write failed during startup shutdown");
    }
    Some(ExitCode::SUCCESS)
}

// --- process helpers (free functions — unit-testable against a real child) ---

/// Spawn `argv` in `workdir` with exactly `env` (stdio inherited). The child is
/// made the leader of its OWN process group (`process_group(0)`), so stop/forward
/// signals can reach the whole tree — a fork-model app's workers must die with it,
/// or the old version's workers would hold the port across an update (P2-16).
/// Returns the child pid; the [`std::process::Child`] is dropped (its `Drop`
/// neither waits nor kills) because the supervisor reaps via `waitpid` to also
/// harvest grandchildren.
fn spawn_process(argv: &[String], workdir: &Path, env: &[(String, String)]) -> Result<Pid> {
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| Error::Process("empty command".to_owned()))?;
    let mut cmd = Command::new(program);
    cmd.args(rest).current_dir(workdir).env_clear();
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    cmd.process_group(0); // child leads its own group (pgid = its pid)
    let child = cmd
        .spawn()
        .map_err(|e| Error::Process(format!("spawn {program}: {e}")))?;
    i32::try_from(child.id())
        .map(Pid::from_raw)
        .map_err(|_| Error::Process("child pid out of range".to_owned()))
}

/// The process-group target for `pid` (its negation, per `kill(2)`), or `None`
/// unless `pid` is a plausible child (raw > 1). Negating 0 or 1 turns a group
/// signal into `kill(0, …)` (lode's OWN process group) or `kill(-1, …)` (every
/// process on the system when lode runs as PID 1) — a torn/stale `state.json`
/// carrying `"pid": 0|1` must never broadcast-kill the container (R2-1).
fn group_target(pid: Pid) -> Option<Pid> {
    (pid.as_raw() > 1).then(|| Pid::from_raw(-pid.as_raw()))
}

/// Signal the child's whole process group (the child leads its own group — see
/// [`spawn_process`]), falling back to the single pid when the group signal fails
/// (e.g. an orphan recorded by an older lode that did not set a process group).
/// Refuses pid <= 1 outright with `ESRCH` — the bare-pid fallback would be just
/// as destructive there (`kill(1, …)` signals init / lode itself) (R2-1).
fn signal_child(pid: Pid, sig: Signal) -> std::result::Result<(), Errno> {
    let group = group_target(pid).ok_or(Errno::ESRCH)?;
    kill(group, sig).or_else(|_| kill(pid, sig))
}

/// Gracefully stop a specific child: `SIGTERM` to its process group, wait up to
/// `timeout` (never killing early), then `SIGKILL` the group. Reaps the child and
/// returns its exit status (`waitpid` still targets the child pid alone).
fn graceful_stop(pid: Pid, timeout: Duration) -> Option<WaitStatus> {
    let _ = signal_child(pid, Signal::SIGTERM);
    let deadline = Instant::now() + timeout;
    loop {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(STOP_POLL);
            }
            Ok(status) => return Some(status),
            Err(_) => return None,
        }
    }
    let _ = signal_child(pid, Signal::SIGKILL);
    waitpid(pid, None).ok()
}

/// Terminate an external process we cannot reap (an orphan re-parented to init):
/// `SIGTERM`, poll liveness up to `timeout`, then `SIGKILL`. Best-effort group
/// first (an orphan from this lode leads its own group), then the bare pid.
fn terminate_external(pid: Pid, timeout: Duration) {
    if signal_child(pid, Signal::SIGTERM).is_err() {
        return; // already gone
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return;
        }
        std::thread::sleep(STOP_POLL);
    }
    let _ = signal_child(pid, Signal::SIGKILL);
}

/// Liveness probe via signal 0: alive unless `kill` reports `ESRCH`.
fn process_alive(pid: Pid) -> bool {
    !matches!(kill(pid, None), Err(Errno::ESRCH))
}

/// Translate a child wait status into a process exit code (`128 + signal` for a
/// signalled child, mirroring the shell convention).
fn exit_code_from(status: WaitStatus) -> u8 {
    match status {
        WaitStatus::Exited(_, code) => u8::try_from(code).unwrap_or(0),
        WaitStatus::Signaled(_, sig, _) => u8::try_from(128 + (sig as i32)).unwrap_or(255),
        _ => 0,
    }
}

/// Exponential backoff for the `attempt`-th restart (0-based): `base * 2^attempt`,
/// capped at `max` (all in seconds), saturating rather than overflowing.
fn backoff_delay(attempt: u32, base_secs: u64, max_secs: u64) -> Duration {
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    Duration::from_secs(base_secs.saturating_mul(factor).min(max_secs))
}

/// Did the child *fail*? Any outcome other than a clean `exit(0)` (a non-zero
/// exit or a fatal signal) counts as a failure for `restart=on-failure`.
const fn is_failure(status: WaitStatus) -> bool {
    !matches!(status, WaitStatus::Exited(_, 0))
}

/// What to do when the supervised child exits while in the `Run` phase (i.e. not
/// a lode-initiated stop). Computed by the pure [`exit_action`] so the policy is
/// unit-testable without spawning processes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExitAction {
    /// A lode-update is pending — apply it and launch the named version. Wins
    /// over the restart policy (covers "app wrote state.target then exit(0)").
    ApplyUpdate(String),
    /// Restart the same version after this backoff delay (restart policy active).
    Restart(Duration),
    /// Keep-alive: the retry cap was reached on a failing app — stop respawning but
    /// stay alive (lode does *not* exit), awaiting a recovery trigger (design §8).
    Pause,
    /// Stop supervising and exit with `code`: mirror the child under `off`, or a
    /// clean `exit(0)` under `on-failure`.
    Exit { code: u8 },
}

/// Decide what a `Run`-phase failure means (a child exit, or a failed start),
/// given the restart `policy`, whether it was a failure (`is_failure`) and the
/// resulting `code`, any resolved `pending_target` (caller-resolved, see
/// [`Supervisor::pending_update`]), the consecutive-retry count so far, and the
/// backoff knobs.
///
/// Order: a pending update always wins; otherwise the policy decides between a
/// backoff restart, mirroring the child (exit with its code), or — once the retry
/// cap is hit — pausing (keep-alive). A `restart_max` of `0` retries forever.
fn exit_action(
    policy: RestartPolicy,
    pending_target: Option<&str>,
    is_failure: bool,
    code: u8,
    restarts: u32,
    restart_max: u32,
    backoff_base_secs: u64,
    backoff_max_secs: u64,
) -> ExitAction {
    if let Some(version) = pending_target {
        return ExitAction::ApplyUpdate(version.to_owned());
    }
    let wants_restart = match policy {
        RestartPolicy::Off => false,
        RestartPolicy::OnFailure => is_failure,
        RestartPolicy::Always => true,
    };
    if !wants_restart {
        return ExitAction::Exit { code };
    }
    // Retry up to `restart_max` times (0 = forever), then pause (keep-alive).
    if restart_max > 0 && restarts + 1 > restart_max {
        return ExitAction::Pause;
    }
    ExitAction::Restart(backoff_delay(restarts, backoff_base_secs, backoff_max_secs))
}

// --- pure update / readiness / rollback decision logic (design §5/§8) ---

/// What an [`update.policy`](crate::config::Policy) check should do with the
/// channel-latest version it just resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PolicyAction {
    /// Nothing to do (policy `off`/pinned, or already up to date).
    Idle,
    /// Advertise the newer version in `state.available` without applying it
    /// (`policy=check`): the app decides whether to request it.
    Advertise(String),
    /// Auto-apply the newer version by setting `state.target` (`policy=auto`).
    Apply(String),
}

/// Is `candidate` a newer version than `current`? Compares by semver precedence
/// when both parse; otherwise treats any *different* id as newer (so a publisher
/// can ship a non-semver channel tag without lode getting stuck, while an
/// unchanged id never re-applies).
fn is_newer(candidate: &str, current: &str) -> bool {
    match (
        semver::Version::parse(candidate),
        semver::Version::parse(current),
    ) {
        (Ok(c), Ok(cur)) => c > cur,
        _ => candidate != current,
    }
}

/// Decide what an update check does, given the policy, whether a `pin` is set, the
/// freshly-fetched channel `latest` and the running `current` version (design §5).
/// A `pin` forces [`PolicyAction::Idle`] (pin acts like `off` + a fixed target).
fn policy_action(policy: Policy, pinned: bool, latest: &str, current: &str) -> PolicyAction {
    if pinned || !is_newer(latest, current) {
        return PolicyAction::Idle;
    }
    match policy {
        Policy::Off => PolicyAction::Idle,
        Policy::Check => PolicyAction::Advertise(latest.to_owned()),
        Policy::Auto => PolicyAction::Apply(latest.to_owned()),
    }
}

/// Is `version` known-bad — i.e. its most recent rollout-`history` entry is
/// `Bad` (a rollback recorded the strike)? A later `Good` entry for the same
/// version clears the verdict. Consulted by the AUTOMATIC update policy only;
/// explicit app/operator `state.target` requests are honoured regardless (P2-11).
fn version_known_bad(st: &State, version: &str) -> bool {
    st.history
        .iter()
        .rev()
        .find(|e| e.version == version)
        .is_some_and(|e| matches!(e.result, HistoryResult::Bad))
}

/// Downgrade an auto-`Apply` of a known-bad version to advertise-only (P2-11):
/// without this gate, `policy=auto` would re-install / crash / roll back a bad
/// channel `latest` on every `check_interval`. The version stays advertised in
/// `state.available` so an explicit `state.target` request can still retry it.
fn gate_policy_action(action: PolicyAction, st: &State) -> PolicyAction {
    match action {
        PolicyAction::Apply(v) if version_known_bad(st, &v) => {
            tracing::warn!(
                version = v,
                "channel latest previously failed and was rolled back; advertising only (write state.target to retry explicitly)"
            );
            PolicyAction::Advertise(v)
        }
        other => other,
    }
}

/// `state.ready` phase suffixes (design §8). The field value is
/// `{LODE_INSTANCE}-{phase}`: the app reports it can serve with `-0`, lode prompts a
/// staged update with `-1`, and the app acks "prepared, cut over now" with `-2`.
const READY_RUNNING: &str = "0";
const READY_PREPARE: &str = "1";
const READY_GO: &str = "2";

/// Compose a `state.ready` token (`{instance}-{phase}`) for the handshake (§8).
fn ready_token(instance: &str, phase: &str) -> String {
    format!("{instance}-{phase}")
}

/// Has the freshly-spawned instance signalled readiness (design §8)? `none` =>
/// alive at least `health_grace`; `state` => the app reported serving for this spawn:
/// the phased token (`{LODE_INSTANCE}-0`) or — for backward compatibility — the bare
/// `LODE_INSTANCE` written by apps that predate the phased handshake.
fn readiness_met(
    mode: Readiness,
    ready: Option<&str>,
    instance: &str,
    alive_for: Duration,
    health_grace: Duration,
) -> bool {
    match mode {
        Readiness::None => alive_for >= health_grace,
        Readiness::State => {
            ready == Some(ready_token(instance, READY_RUNNING).as_str()) || ready == Some(instance)
        }
    }
}

/// The outcome of one observation tick on a freshly-applied update target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObserveOutcome {
    /// Keep observing.
    Pending,
    /// The new version is healthy — commit it as `last_good`.
    Commit,
    /// The new version failed — roll back to the previous `last_good`.
    Rollback,
}

/// Fold one observation tick into an outcome: readiness wins (commit); a readiness
/// timeout triggers a rollback; else keep waiting. A crash within the grace window
/// is handled separately (single-strike rollback in [`Supervisor::on_observe_exit`]).
const fn observe_decision(ready: bool, timed_out: bool) -> ObserveOutcome {
    if ready {
        ObserveOutcome::Commit
    } else if timed_out {
        ObserveOutcome::Rollback
    } else {
        ObserveOutcome::Pending
    }
}

/// What a bumped `restart_nonce` does, by supervisor situation (P2-13): it must
/// act in EVERY phase — `lode-cli restart` during a staged prepare or an
/// observation window must not be silently swallowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NonceAction {
    /// Paused: the bump is the recovery trigger — resume (respawn).
    Resume,
    /// Normal supervision: graceful-restart the child.
    Restart,
    /// Mid-prepare: abandon the staged prepare (clear the `-1` prompt, return to
    /// `Run`) then graceful-restart. The pending `state.target` survives, so the
    /// update is re-staged once the restarted app reports serving again.
    AbandonPrepareAndRestart,
    /// Mid-observation: graceful-restart the observed child but KEEP the
    /// observation (phase + deadline) — the window judges the applied VERSION,
    /// not one process, so a restart must not extend the rollback deadline.
    RestartObserved,
}

/// Decide what a bumped `restart_nonce` does given the pause flag and phase.
const fn nonce_action(paused: bool, phase: &Phase) -> NonceAction {
    if paused {
        NonceAction::Resume
    } else {
        match phase {
            Phase::Run => NonceAction::Restart,
            Phase::Prepare(_) => NonceAction::AbandonPrepareAndRestart,
            Phase::Observe(_) => NonceAction::RestartObserved,
        }
    }
}

/// Should a staged prepare cut over now (P2-13)? The app's `-2` ack always wins;
/// otherwise a configured `prepare_timeout` (seconds; 0 = disabled — the app
/// paces the cut-over, the documented default) forces it once exceeded, so an
/// app that never acks cannot wedge the staged update forever.
const fn prepare_cutover_due(acked: bool, elapsed: Duration, timeout_secs: u64) -> bool {
    acked || (timeout_secs > 0 && elapsed.as_secs() >= timeout_secs)
}

/// Append a rollout-history entry, bounding the vector to [`HISTORY_CAP`] (oldest
/// dropped first) so `state.json` stays small.
fn push_history(history: &mut Vec<HistoryEntry>, version: &str, result: HistoryResult, at: String) {
    history.push(HistoryEntry {
        version: version.to_owned(),
        at,
        result,
    });
    if history.len() > HISTORY_CAP {
        let overflow = history.len() - HISTORY_CAP;
        history.drain(0..overflow);
    }
}

/// Current wall-clock time as an RFC 3339 UTC timestamp (`YYYY-MM-DDThh:mm:ssZ`),
/// used for `state.last_check` and `history[].at`. Falls back to the epoch if the
/// clock is before `UNIX_EPOCH` (never panics).
fn now_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_rfc3339(secs)
}

/// Format `epoch_secs` (seconds since the Unix epoch, UTC) as `YYYY-MM-DDThh:mm:ssZ`.
#[allow(clippy::cast_possible_wrap)] // epoch seconds stay far within i64 range
fn format_rfc3339(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem = epoch_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Convert a count of days since the Unix epoch into a civil `(year, month, day)`
/// (Howard Hinnant's algorithm; proleptic Gregorian, valid for all realistic
/// timestamps).
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)] // bounded sub-results
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (year + i64::from(month <= 2), month, day)
}

// --- supervisor ---

/// Where the supervisor is in the update lifecycle.
enum Phase {
    /// Normal supervision: crash-restart + update polling (design §5/§8).
    Run,
    /// A target is staged and installed; the running app has been prompted
    /// (`state.ready = {instance}-1`) and lode is waiting for it to ack "prepared"
    /// (`-2`) before cutting over. Only used under `readiness=state` (design §8).
    Prepare(Prepare),
    /// Observing a freshly-applied target for readiness + stability before
    /// committing it as `last_good`, or rolling back on failure (design §5).
    Observe(Observe),
}

/// State carried while waiting for the app's go-ahead on a staged update (§8). The
/// old child keeps serving (and is supervised) throughout; a crash here applies the
/// staged target directly via update-on-exit, since there is nothing left to drain.
struct Prepare {
    /// The version staged and awaiting the app's `-2` ack.
    target: String,
    /// When the prompt was issued — drives the optional `prepare_timeout`
    /// forced cut-over (0 = disabled, app-paced; P2-13).
    started: Instant,
}

/// State carried while observing a freshly-applied (or rolled-back) version.
/// Rollback is single-strike: any exit within the grace window, or a readiness
/// timeout, fails the observation (design §5).
struct Observe {
    /// The version being observed (now `current`).
    applied: String,
    /// The version to roll back to on failure (the one we replaced), or `None`
    /// when this *is* the rollback observation (`applied` == `last_good`): with
    /// no further fallback, a failure here makes lode exit.
    fallback: Option<String>,
    /// Deadline for the readiness handshake (`readiness=state`, design §8).
    deadline: Instant,
}

/// Owns the supervise loop state for one served version.
struct Supervisor<'c> {
    cfg: &'c Config,
    target: Target,
    runtime_dir: Option<PathBuf>,
    forward: Vec<Signal>,
    restart: Option<Signal>,
    /// The live child, or `None` while backing off before a restart.
    child: Option<Pid>,
    /// When the current child was spawned (to reset the backoff after `grace`).
    spawn_at: Instant,
    /// Consecutive crash/start restarts in the current crash-loop.
    restart_count: u32,
    /// When to re-spawn after a backoff (`None` once spawned).
    restart_at: Option<Instant>,
    /// Keep-alive pause: the app exhausted its retries, so lode stays alive (PID 1
    /// does not exit) and stops respawning until a recovery trigger — an edited
    /// `lode.toml`, a bumped `restart_nonce`, or a new `target` (design §8).
    paused: bool,
    /// `lode.toml`'s mtime captured when the app paused; while paused, a change
    /// triggers a config reload + re-attempt. A *running* app is never disturbed.
    last_config_mtime: Option<SystemTime>,
    /// The current child's `LODE_INSTANCE` (`{pid}-{nanoid}`), the unique half of
    /// the `state.ready` handshake (design §8). A fresh random token per spawn means
    /// a stale `state.ready` — even from a lode that reused this OS pid — can never
    /// satisfy a new spawn's readiness check.
    instance: String,
    /// Update lifecycle: normal supervision, preparing a staged target, or
    /// observing an applied one.
    phase: Phase,
    /// When `state.json`'s mtime was last polled (`None` => never).
    last_state_poll: Option<Instant>,
    /// The mtime observed at the last poll, to skip re-reads when unchanged.
    last_state_mtime: Option<SystemTime>,
    /// The highest `restart_nonce` already serviced (so each bump acts once).
    last_nonce: u64,
    /// When the next policy update check is due (`None` => no further checks).
    next_check_at: Option<Instant>,
}

impl<'c> Supervisor<'c> {
    fn new(cfg: &'c Config, target: Target, runtime_dir: Option<PathBuf>) -> Self {
        // Seed the serviced nonce from any existing state so a pre-existing
        // `restart_nonce` does not trigger a spurious restart on startup.
        let last_nonce = state::read_lenient(&cfg.global.data_dir.join("state.json"))
            .map_or(0, |st| st.restart_nonce);
        // Schedule the first update check immediately for check/auto (unless
        // pinned); `off`/pinned never checks (design §5).
        let next_check_at = if cfg.update.pin.is_some() || matches!(cfg.update.policy, Policy::Off)
        {
            None
        } else {
            Some(Instant::now())
        };
        Self {
            forward: forward_signals(&cfg.signals.forward),
            restart: cfg.signals.restart.as_deref().and_then(parse_signal),
            cfg,
            target,
            runtime_dir,
            child: None,
            spawn_at: Instant::now(),
            restart_count: 0,
            restart_at: None,
            paused: false,
            last_config_mtime: None,
            instance: String::new(),
            phase: Phase::Run,
            last_state_poll: None,
            last_state_mtime: None,
            last_nonce,
            next_check_at,
        }
    }

    /// Run the supervise loop until a graceful shutdown (`Outcome::Exit`) or a
    /// `lode.toml`-change recovery while paused (`Outcome::Reload`). A failing app is
    /// retried then PAUSED (lode stays alive) — it never exits on app failure.
    fn run(&mut self, signals: &mut Signals) -> Result<Outcome> {
        // The process-wide signal set was installed at `serve` start (so bootstrap
        // is interruptible); (re-)add this config's set — a reload may have changed
        // `[signals]`, and adding an already-registered signal is a no-op. Stale
        // registrations from a previous config are harmless: [`classify`] ignores
        // signals outside the current sets.
        for raw in registration_set(&self.forward, self.restart) {
            signals
                .add_signal(raw)
                .map_err(|e| Error::Process(format!("install signal handlers: {e}")))?;
        }

        // v1 implements `stop-start` fully; the zero-downtime modes are optional /
        // out of scope (design §8) and fall back to stop-start with this note.
        if !matches!(
            self.cfg.supervise.restart_mode,
            crate::config::RestartMode::StopStart
        ) {
            tracing::info!(
                mode = ?self.cfg.supervise.restart_mode,
                "restart_mode is not yet supported; using stop-start (v1 default, design §8)"
            );
        }

        // Watermark the lode.toml mtime from the start so a later edit is detected
        // WHILE the app runs (design §7): a running edit notifies the app (bumps
        // state.config_generation) and never auto-restarts; only a paused edit reloads.
        self.last_config_mtime = self
            .cfg
            .config_path
            .as_deref()
            .and_then(|p| state::mtime(p).ok().flatten());

        self.set_status(Status::Starting);
        if let Some(outcome) = self.spawn_supervised() {
            return Ok(outcome);
        }

        loop {
            for raw in signals.pending() {
                let Ok(sig) = Signal::try_from(raw) else {
                    continue;
                };
                match classify(sig, self.restart, &self.forward) {
                    Action::Terminate => return Ok(Outcome::Exit(self.shutdown())),
                    // A restart request resumes a paused app; otherwise it cycles
                    // the running child.
                    Action::Restart if self.paused => self.resume(),
                    Action::Restart => return Ok(self.graceful_restart_reload()),
                    Action::Forward => {
                        if let Some(pid) = self.child {
                            // Forward to the whole process group so a fork-model
                            // app's workers see the signal too (P2-16).
                            let _ = signal_child(pid, sig);
                        }
                    }
                    Action::Ignore => {}
                }
            }

            if let Some(status) = self.reap()
                && let Some(outcome) = self.on_child_exit(status)
            {
                return Ok(outcome);
            }

            if !self.paused
                && self.child.is_none()
                && self.restart_at.is_some_and(|at| Instant::now() >= at)
            {
                self.restart_at = None;
                if let Some(outcome) = self.spawn_supervised() {
                    return Ok(outcome);
                }
            }

            // C2: honour app-written requests, run the policy update check, and
            // drive the readiness/rollback observation of an applied target.
            if let Some(outcome) = self.poll_state() {
                return Ok(outcome);
            }
            // An edited `lode.toml`: while PAUSED it's the operator's "fixed it — try
            // again" → reload + re-attempt. While RUNNING lode never auto-restarts (a
            // running app is never disturbed) — it only NOTIFIES the app (bumps
            // state.config_generation) so the app can request a restart at its own pace
            // (bump restart_nonce, which re-reads lode.toml) to apply the change (§7).
            if self.config_changed() {
                if self.paused {
                    tracing::info!("lode.toml changed while paused; reloading and re-attempting");
                    return Ok(Outcome::Reload);
                }
                self.notify_config_changed();
            }
            if !self.paused {
                self.maybe_check_update();
            }
            if let Some(outcome) = self.poll_prepare() {
                return Ok(outcome);
            }
            if let Some(outcome) = self.poll_observe() {
                return Ok(outcome);
            }

            std::thread::sleep(POLL_TICK);
        }
    }

    /// Launch the child process for the current `target`, recording its pid,
    /// spawn time and `LODE_INSTANCE`. Does *not* touch `state.json` — the caller
    /// writes the phase-appropriate status afterwards.
    fn spawn_child(&mut self) -> Result<Pid> {
        let instance = format!("{}-{}", std::process::id(), nanoid());
        let dir = self.target.dir.to_string_lossy();
        let run = effective_command(
            self.target.run.as_deref(),
            self.cfg.command.run.as_deref(),
            "run",
        )?;
        let argv = build_run_argv(&run, &dir)?;
        let mut env = child_env(
            std::env::vars(),
            &self.cfg.env,
            &self.target.version,
            &self.cfg.global.data_dir,
            &instance,
            self.runtime_dir.as_deref(),
        );
        // Tell the app which readiness contract is in force so it knows whether to
        // run the `state.ready` handshake (design §8); a self-introspection var,
        // like the other `LODE_*` injected above.
        env.push((
            "LODE_READINESS".to_owned(),
            readiness_label(self.cfg.supervise.readiness).to_owned(),
        ));
        let workdir = PathBuf::from(expand_token(&self.cfg.command.workdir, &dir));

        let pid = spawn_process(&argv, &workdir, &env)?;
        self.child = Some(pid);
        self.spawn_at = Instant::now();
        self.instance.clone_from(&instance);
        tracing::info!(
            version = self.target.version,
            pid = pid.as_raw(),
            instance,
            "spawned child"
        );
        Ok(pid)
    }

    /// Spawn the current version and write the phase-appropriate state (`running`
    /// while supervising, `updating` while observing). On a spawn failure, apply the
    /// restart policy (backoff retry, or pause under keep-alive) instead of
    /// propagating — an app that cannot even start must not take down lode (design
    /// §8). Returns `Some(Outcome)` only when lode must stop now (mirror exit / clean
    /// exit under `off`).
    fn spawn_supervised(&mut self) -> Option<Outcome> {
        // An Observe-phase (re)spawn re-arms the readiness handshake: the
        // `ready`/`target` clear must land BEFORE the child exists, or a fast
        // child's `-0` serving token could be clobbered (P2-14, see
        // `write_pre_observe_state`); only the pid is written after the spawn.
        if matches!(self.phase, Phase::Observe(_)) {
            self.write_pre_observe_state();
        }
        match self.spawn_child() {
            Ok(pid) => {
                match self.phase {
                    Phase::Observe(_) => self.record_child_pid(pid),
                    // A `Prepare`-phase respawn is not reached in practice (a crash
                    // there applies the staged target via update-on-exit).
                    Phase::Run | Phase::Prepare(_) => self.write_running_state(pid),
                }
                self.paused = false;
                None
            }
            Err(e) => self.on_spawn_failure(&e),
        }
    }

    /// A failed start (the app could not be exec'd). Treated as a failure for the
    /// restart policy: back off and retry, then pause (keep-alive) — or mirror-exit
    /// under `off` — once the retry cap is reached.
    fn on_spawn_failure(&mut self, e: &Error) -> Option<Outcome> {
        tracing::error!(error = %e, version = self.target.version, "failed to start app");
        self.note_error(&format!("start {}: {e}", self.target.version));
        match exit_action(
            self.cfg.supervise.restart,
            None,
            true, // a failed start is always a failure
            1,
            self.restart_count,
            self.cfg.supervise.restart_max,
            self.cfg.supervise.restart_backoff,
            self.cfg.supervise.restart_backoff_max,
        ) {
            ExitAction::Restart(delay) => {
                self.restart_count = self.restart_count.saturating_add(1);
                self.child = None;
                self.restart_at = Some(Instant::now() + delay);
                let backoff_secs = delay.as_secs();
                tracing::warn!(
                    restart = self.restart_count,
                    backoff_secs,
                    "app failed to start; backing off"
                );
                self.set_status(Status::Error);
                None
            }
            ExitAction::Pause => {
                self.enter_paused(1);
                None
            }
            ExitAction::Exit { code } => {
                self.set_error(&format!("app failed to start: {e}"));
                Some(Outcome::Exit(ExitCode::from(code)))
            }
            ExitAction::ApplyUpdate(_) => unreachable!("no pending target passed"),
        }
    }

    /// Keep-alive: an unrecoverable-by-retry failure pauses (stay alive) under a
    /// restart policy, or mirror-exits under `off`.
    fn pause_or_exit(&mut self, code: u8) -> Option<Outcome> {
        if matches!(self.cfg.supervise.restart, RestartPolicy::Off) {
            self.set_error(&format!("app failed (exit {code})"));
            Some(Outcome::Exit(ExitCode::from(code)))
        } else {
            self.enter_paused(code);
            None
        }
    }

    /// Enter the keep-alive pause: stop respawning and stay alive (PID 1 does NOT
    /// exit) until a recovery trigger — an edited `lode.toml`, a bumped
    /// `restart_nonce`, or a new `target` (design §8). Captures `lode.toml`'s mtime
    /// so only a *later* edit (the operator's fix) triggers a reload. The in-memory
    /// pause takes effect even when the (best-effort) state write fails — a full
    /// disk must not defeat the keep-alive.
    fn enter_paused(&mut self, code: u8) {
        let attempts = self.restart_count;
        tracing::error!(
            version = self.target.version,
            attempts,
            "app failed to stay up — pausing (lode stays alive); recover by editing lode.toml, bumping restart_nonce, or setting a new target"
        );
        self.paused = true;
        self.child = None;
        self.restart_at = None;
        // Paused means the rollout machinery is dead: drop any in-flight
        // prepare/observation so a recovery respawns under normal supervision
        // (never against a stale prompt or rollback deadline).
        self.phase = Phase::Run;
        self.last_config_mtime = self
            .cfg
            .config_path
            .as_deref()
            .and_then(|p| state::mtime(p).ok().flatten());
        self.mutate_state(|st| {
            st.status = Some(Status::Error);
            st.pid = None;
            st.last_error = Some(format!(
                "paused after {attempts} failed start attempts (last exit {code})"
            ));
        });
    }

    /// Resume a paused app: clear the pause, reset the retry count, and respawn
    /// promptly (the run loop spawns once `restart_at` is due).
    fn resume(&mut self) {
        tracing::info!(version = self.target.version, "resuming paused app");
        self.paused = false;
        self.restart_count = 0;
        self.restart_at = Some(Instant::now());
    }

    /// Has `lode.toml` changed since the app paused? Only consulted while paused, so
    /// a running app is never disturbed by edits (design §8). Always false when
    /// running file-less (no config path). Best-effort: a probe error (EIO,
    /// transient EACCES) is "unchanged", never an exit of PID 1 (R2-2).
    fn config_changed(&self) -> bool {
        let Some(path) = self.cfg.config_path.as_deref() else {
            return false;
        };
        match state::mtime(path) {
            Ok(mtime) => mtime != self.last_config_mtime,
            Err(e) => {
                tracing::warn!(error = %e, "cannot stat lode.toml; treating as unchanged");
                false
            }
        }
    }

    /// Reap our child plus any re-parented grandchildren (subreaper). Returns the
    /// supervised child's status if it exited this pass, else `None`.
    fn reap(&mut self) -> Option<WaitStatus> {
        let mut child_status = None;
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                // No more children have changed state, or none remain (ECHILD).
                Ok(WaitStatus::StillAlive) | Err(_) => break,
                Ok(status) => {
                    if status.pid() == self.child {
                        self.child = None;
                        child_status = Some(status);
                    }
                    // else: a grandchild we adopted — reaped and discarded.
                }
            }
        }
        child_status
    }

    /// Handle a child exit reaped in the run loop (i.e. *not* a lode-initiated
    /// stop). Returns `Some(code)` when lode should exit with that code, or `None`
    /// after scheduling a restart / applying an update / rolling back.
    ///
    /// A pending update always wins (update-on-exit). Otherwise the
    /// `supervise.restart` policy decides between a bounded-backoff restart and
    /// mirroring the child (exit with its code). While observing a freshly-applied
    /// version this is a single-strike rollback (design §5).
    fn on_child_exit(&mut self, status: WaitStatus) -> Option<Outcome> {
        if matches!(self.phase, Phase::Observe(_)) {
            return self.on_observe_exit(status);
        }

        // A child that survived the grace window starts a fresh restart sequence.
        if self.spawn_at.elapsed() >= Duration::from_secs(self.cfg.supervise.health_grace) {
            self.restart_count = 0;
        }

        let pending = self.pending_update();
        let code = exit_code_from(status);
        match exit_action(
            self.cfg.supervise.restart,
            pending.as_deref(),
            is_failure(status),
            code,
            self.restart_count,
            self.cfg.supervise.restart_max,
            self.cfg.supervise.restart_backoff,
            self.cfg.supervise.restart_backoff_max,
        ) {
            ExitAction::ApplyUpdate(version) => {
                tracing::info!(version, "child exited with an update pending; applying");
                if let Some(outcome) = self.apply_target(&version) {
                    return Some(outcome);
                }
                if self.child.is_some() || self.restart_at.is_some() || self.paused {
                    // Observing the new version (or its rollback), retrying a spawn
                    // with backoff, or already paused by the failure machinery.
                    return None;
                }
                // The update could not be started — keep-alive: pause (don't exit).
                tracing::error!(version, "pending update could not be started");
                self.pause_or_exit(if code == 0 { 1 } else { code })
            }
            ExitAction::Restart(delay) => {
                self.schedule_restart(status, delay);
                None
            }
            ExitAction::Pause => {
                self.enter_paused(code);
                None
            }
            ExitAction::Exit { code } => {
                self.finish_exit(code);
                Some(Outcome::Exit(ExitCode::from(code)))
            }
        }
    }

    /// Record a scheduled backoff restart of the same version (`restart` policy
    /// active). The new child is spawned by the run loop once `restart_at` is due.
    fn schedule_restart(&mut self, status: WaitStatus, delay: Duration) {
        self.restart_count = self.restart_count.saturating_add(1);
        let code = exit_code_from(status);
        let backoff_secs = delay.as_secs();
        tracing::warn!(
            version = self.target.version,
            code,
            restart = self.restart_count,
            backoff_secs,
            "child exited; scheduling restart"
        );
        self.restart_at = Some(Instant::now() + delay);
    }

    /// Stop supervising and exit (the `off` mirror, or a clean `exit(0)`): write the
    /// terminal status — `stopped` on a clean exit, `error` on a failing one. A
    /// failing app under a keep-alive policy pauses instead (see [`Self::enter_paused`]).
    fn finish_exit(&self, code: u8) {
        if code == 0 {
            tracing::info!(
                version = self.target.version,
                "child exited cleanly; lode exiting"
            );
            self.set_stopped();
        } else {
            tracing::error!(
                version = self.target.version,
                code,
                "child exited; lode exiting"
            );
            self.set_error(&format!("child exited with code {code}"));
        }
    }

    /// Resolve a lode-update to apply when the child exits: an app/auto-written
    /// `state.target` naming a different version (the documented `"latest"` alias
    /// re-resolves through the channel pointer first), or — under `policy=auto` —
    /// a channel latest newer than current. Best-effort; IO failures yield `None`,
    /// and an unresolvable `"latest"` falls through so a cleanly-exiting app is
    /// never paused over a transient manifest error.
    fn pending_update(&self) -> Option<String> {
        let path = self.cfg.global.data_dir.join("state.json");
        let st = state::read_lenient(&path);
        if let Some(target) = st.as_ref().and_then(|s| s.target.as_deref())
            && target != self.target.version
        {
            match self.resolve_request(target) {
                Some(version) if version != self.target.version => return Some(version),
                Some(_) => {} // `latest` points at the running version — nothing to apply
                None => {
                    tracing::warn!(target, "cannot resolve pending update target; ignoring");
                }
            }
        }
        if matches!(self.cfg.update.policy, Policy::Auto)
            && self.cfg.update.pin.is_none()
            && let Some(latest) = self.resolve_latest()
            && is_newer(&latest, &self.target.version)
        {
            // P2-11: the automatic policy never re-applies a known-bad latest
            // (the explicit `state.target` branch above still honours it).
            if st.as_ref().is_some_and(|s| version_known_bad(s, &latest)) {
                tracing::warn!(
                    latest,
                    "channel latest previously failed and was rolled back; not auto-applying on exit"
                );
                return None;
            }
            return Some(latest);
        }
        None
    }

    /// Map a `state.target` request onto a concrete version: the documented
    /// `"latest"` alias (docs/integration.md §2) re-resolves through the channel
    /// pointer; anything else already names an exact version. `None` only when
    /// `latest` cannot be resolved (no manifest source / fetch / parse error).
    fn resolve_request(&self, target: &str) -> Option<String> {
        if target == "latest" {
            self.resolve_latest()
        } else {
            Some(target.to_owned())
        }
    }

    /// Resolve the channel-latest version from a freshly-fetched manifest
    /// (best-effort; any fetch/parse/mismatch error yields `None`).
    fn resolve_latest(&self) -> Option<String> {
        let manifest = manifest::fetch(self.cfg).ok()?;
        if manifest.name != self.cfg.global.app {
            return None;
        }
        // No `floor` here: the periodic poll's own `is_newer` gate (against the
        // running version) already refuses a downgrade, so resolving the raw latest
        // and filtering downstream keeps a benign older `latest` from logging as an
        // error.
        manifest::resolve_target(
            &manifest,
            &self.cfg.update.channel,
            self.cfg.update.pin.as_deref(),
            Some("latest"),
            None,
        )
        .ok()
    }

    /// Handle a child exit while observing a freshly-activated version: a single
    /// strike rolls back to the fallback (or pauses if this *was* the rollback).
    fn on_observe_exit(&mut self, status: WaitStatus) -> Option<Outcome> {
        let code = exit_code_from(status);
        tracing::warn!(
            version = self.target.version,
            code,
            "freshly-activated version exited within the grace window"
        );
        self.observe_failed("crashed within health grace", Some(status))
    }

    /// Graceful restart (configured restart signal / `restart_nonce`): stop the
    /// child, reset the backoff and re-spawn immediately. A spawn failure routes
    /// through the keep-alive policy (`Some(Outcome)` only on a mirror exit).
    fn graceful_restart(&mut self) -> Option<Outcome> {
        tracing::info!(version = self.target.version, "graceful restart requested");
        if self.child.is_some() {
            self.set_status(Status::Stopping);
            self.stop_child();
        }
        self.restart_count = 0;
        self.restart_at = None;
        self.spawn_supervised()
    }

    /// A normal (`Run`-phase) restart request — from `restart_nonce` or the configured
    /// restart signal. Unlike [`Self::graceful_restart`] (which re-spawns the same
    /// version in place), this stops the child and returns [`Outcome::Reload`] so
    /// `serve` RE-READS `lode.toml`, applying any edited `[env]`/config on relaunch
    /// (design §7 — config changes reach a running app via "edit → app bumps nonce →
    /// reload"). A restart during a staged-update prepare/observation uses the
    /// in-place path instead, to preserve rollout safety; config is then applied on
    /// the next normal restart.
    fn graceful_restart_reload(&mut self) -> Outcome {
        tracing::info!(
            version = self.target.version,
            "restart requested; reloading lode.toml and relaunching"
        );
        if self.child.is_some() {
            self.set_status(Status::Stopping);
            self.stop_child();
        }
        Outcome::Reload
    }

    /// A running app's `lode.toml` was edited. lode does NOT restart (a running app is
    /// never disturbed by an edit) — it advances the mtime watermark and bumps
    /// `state.config_generation` so the app learns a restart is needed to apply the
    /// change. The app applies it at its own pace by bumping `restart_nonce` (which
    /// reloads `lode.toml`). Best-effort: a state-write failure never kills PID 1.
    fn notify_config_changed(&mut self) {
        self.last_config_mtime = self
            .cfg
            .config_path
            .as_deref()
            .and_then(|p| state::mtime(p).ok().flatten());
        self.mutate_state(|st| st.config_generation = st.config_generation.saturating_add(1));
        tracing::info!(
            "lode.toml changed while the app is running; notified the app via \
             state.config_generation (no auto-restart — bump restart_nonce to apply)"
        );
    }

    /// Graceful shutdown: stop the child and exit with its code.
    fn shutdown(&mut self) -> ExitCode {
        tracing::info!("termination signal received; stopping child");
        self.set_status(Status::Stopping);
        let code = self.stop_child().map_or(0, exit_code_from);
        self.set_stopped();
        ExitCode::from(code)
    }

    /// Stop the current child (if any), returning its exit status.
    fn stop_child(&mut self) -> Option<WaitStatus> {
        let pid = self.child.take()?;
        graceful_stop(pid, Duration::from_secs(self.cfg.supervise.stop_timeout))
    }

    // --- state.json (read-modify-write, preserving app-owned fields) ---

    fn write_running_state(&self, pid: Pid) {
        let pid_u32 = u32::try_from(pid.as_raw()).ok();
        let version = self.target.version.clone();
        self.mutate_state(|st| {
            st.status = Some(Status::Running);
            st.current = Some(version.clone());
            if st.last_good.is_none() {
                st.last_good = Some(version);
            }
            st.pid = pid_u32;
        });
    }

    fn set_status(&self, status: Status) {
        self.mutate_state(|st| st.status = Some(status));
    }

    fn set_stopped(&self) {
        self.mutate_state(|st| {
            st.status = Some(Status::Stopped);
            st.pid = None;
        });
    }

    fn set_error(&self, message: &str) {
        let message = message.to_owned();
        self.mutate_state(|st| {
            st.status = Some(Status::Error);
            st.last_error = Some(message);
            st.pid = None;
        });
    }

    /// Best-effort read-modify-write of `state.json`, preserving app-owned fields,
    /// serialised against concurrent RMWs (CLI commands, contract-honouring apps)
    /// via the sibling `state.json.lock` flock ([`state::locked_update_lenient`],
    /// P2-14). The blocking lock wait is deliberate: critical sections are one
    /// read plus one atomic write (microseconds), far below the 200ms loop tick.
    /// Failures are logged, never propagated: `state.json` is the advisory comms
    /// channel, and a full or read-only disk must not take down PID 1 (keep-alive,
    /// design §8) — the supervise loop carries on with its in-memory state. The
    /// strict CLI command paths use [`state::locked_update`] instead.
    fn mutate_state(&self, edit: impl FnOnce(&mut State)) {
        let path = self.cfg.global.data_dir.join("state.json");
        state::locked_update_lenient(&path, edit);
    }

    /// Pre-spawn half of the observing-state write: report `updating` on the new
    /// `current`, consume the `target` request that triggered the apply, and clear
    /// `ready` so the fresh spawn's serving token (`{instance}-0`) — not the old
    /// prepare prompt / ack — is what gates the readiness handshake (§8). MUST run
    /// BEFORE `spawn_child`: once the child exists it can write its `-0` token at
    /// any moment, and clearing `ready` after the spawn could clobber it →
    /// spurious readiness-timeout rollback (P2-14). `pid` is cleared here (the old
    /// child is stopped by now) and recorded post-spawn by [`Self::record_child_pid`].
    fn write_pre_observe_state(&self) {
        let version = self.target.version.clone();
        self.mutate_state(|st| {
            st.status = Some(Status::Updating);
            st.current = Some(version);
            st.pid = None;
            st.target = None;
            st.ready = None;
        });
    }

    /// Post-spawn half: record the fresh child's pid. A field-preserving RMW
    /// under the state lock, so a `-0` serving token the child has already
    /// written survives (§8 / P2-14).
    fn record_child_pid(&self, pid: Pid) {
        let pid_u32 = u32::try_from(pid.as_raw()).ok();
        self.mutate_state(|st| st.pid = pid_u32);
    }

    /// Record a non-fatal `last_error` without disturbing `status`/`pid` (the
    /// child keeps running on the current version).
    fn note_error(&self, message: &str) {
        let message = message.to_owned();
        self.mutate_state(|st| st.last_error = Some(message));
    }

    /// Clear a consumed `target` request from `state.json`.
    fn clear_target(&self) {
        self.mutate_state(|st| st.target = None);
    }

    // --- C2: app-request poll, update policy, apply / observe / rollback (§5/§7/§8) ---

    /// Poll `state.json`'s mtime (~1s) and, on a change, honour app-written
    /// requests: a bumped `restart_nonce` or a new `target`. While paused these are
    /// recovery triggers (resume / apply); while running they cycle / hot-update the
    /// child. Returns `Some(Outcome)` only when the action stops lode (design §7/§8).
    /// Infallible: `state.json` problems skip the tick, never exit PID 1 (R2-2).
    fn poll_state(&mut self) -> Option<Outcome> {
        if !self.state_poll_due() {
            return None;
        }
        self.last_state_poll = Some(Instant::now());
        let path = self.cfg.global.data_dir.join("state.json");
        // Best-effort: a probe error (EIO, transient EACCES) skips the tick and
        // retries on the next — never an exit of PID 1 (R2-2).
        let mtime = match state::mtime(&path) {
            Ok(mtime) => mtime,
            Err(e) => {
                tracing::warn!(error = %e, "cannot stat state.json; skipping poll tick");
                return None;
            }
        };
        if mtime == self.last_state_mtime {
            return None;
        }
        self.last_state_mtime = mtime;

        // Lenient: an app could tear a write at any poll — warn + quarantine and
        // skip the tick rather than kill PID 1 mid-run.
        let st = state::read_lenient(&path)?;

        // A bumped restart nonce is high-water-marked so it acts exactly once —
        // and it acts in EVERY phase (P2-13): a restart requested mid-prepare or
        // mid-observation must not be silently swallowed.
        if st.restart_nonce > self.last_nonce {
            self.last_nonce = st.restart_nonce;
            let nonce = st.restart_nonce;
            match nonce_action(self.paused, &self.phase) {
                NonceAction::Resume => {
                    tracing::info!(nonce, "restart requested; resuming paused app");
                    self.resume();
                }
                NonceAction::Restart => {
                    tracing::info!(nonce, "restart requested via state.json");
                    return Some(self.graceful_restart_reload());
                }
                NonceAction::AbandonPrepareAndRestart => {
                    // Drop the staged prepare (and its `-1` prompt) first; the
                    // pending `state.target` survives, so the update re-stages
                    // once the restarted app reports serving again.
                    tracing::info!(
                        nonce,
                        "restart requested mid-prepare; abandoning staged prepare"
                    );
                    self.abandon_prepare();
                    return self.graceful_restart();
                }
                NonceAction::RestartObserved => {
                    // Restart the observed child but KEEP the observation window
                    // (phase + deadline): it judges the applied VERSION, not one
                    // process — a restart must not extend the rollback deadline.
                    tracing::info!(
                        nonce,
                        "restart requested mid-observation; restarting observed child"
                    );
                    return self.graceful_restart();
                }
            }
            return None;
        }

        // A target different from the running version is a hot-update request. The
        // documented `"latest"` alias re-resolves through the channel pointer first
        // (docs/integration.md §2); an unresolvable alias keeps the current version
        // running and drops the request. While paused, a new target un-pauses and is
        // applied directly (the app is down, so there is no prepare handshake). While
        // running, an app that opted into the prepare handshake (serving token
        // `{instance}-0`) gets staged + prompted (§8); everything else cuts over
        // immediately. Either path consumes `target`.
        if let Some(target) = st.target.as_deref()
            && target != self.target.version
            && (self.paused || matches!(self.phase, Phase::Run))
        {
            let Some(version) = self.resolve_request(target) else {
                tracing::warn!(target, "cannot resolve update target; staying on current");
                self.note_error(&format!("resolve {target}: cannot resolve channel latest"));
                self.clear_target();
                return None;
            };
            if version == self.target.version {
                // `latest` already points at the running version — consume the request.
                self.clear_target();
            } else if self.paused {
                self.paused = false;
                self.restart_count = 0;
                let outcome = self.apply_target(&version);
                // P2-12: only LEAVE the pause when the apply actually produced a
                // child / a scheduled retry / a (re-)pause. An uninstallable
                // target would otherwise strand lode un-paused with no child and
                // no restart_at — and `config_changed()` is gated on `paused`,
                // so the documented lode.toml-edit recovery would stop working.
                if outcome.is_none()
                    && self.child.is_none()
                    && self.restart_at.is_none()
                    && !self.paused
                {
                    tracing::warn!(
                        version,
                        "recovery target could not be applied; staying paused"
                    );
                    self.enter_paused(1);
                    self.note_error(&format!(
                        "recovery target {version} could not be applied; still paused"
                    ));
                }
                return outcome;
            } else {
                let prepares = matches!(self.cfg.supervise.readiness, Readiness::State)
                    && st.ready.as_deref()
                        == Some(ready_token(&self.instance, READY_RUNNING).as_str());
                if prepares {
                    self.begin_prepare(&version);
                } else {
                    return self.apply_target(&version);
                }
            }
        }
        None
    }

    /// Is the ~1s `state.json` poll due?
    fn state_poll_due(&self) -> bool {
        self.last_state_poll
            .is_none_or(|t| t.elapsed() >= STATE_POLL_INTERVAL)
    }

    /// Run the policy update check when due, then schedule the next one.
    fn maybe_check_update(&mut self) {
        if !self.update_check_due() {
            return;
        }
        self.run_update_check();
        self.schedule_next_check();
    }

    /// Is a policy update check due?
    fn update_check_due(&self) -> bool {
        self.next_check_at.is_some_and(|at| Instant::now() >= at)
    }

    /// Schedule the next check: never (`check_interval=0` => once at startup), or
    /// `check_interval` seconds out.
    fn schedule_next_check(&mut self) {
        self.next_check_at = if self.cfg.update.check_interval == 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_secs(self.cfg.update.check_interval))
        };
    }

    /// Fetch the manifest and apply the `[update].policy`: `check` advertises a
    /// newer version in `state.available`; `auto` sets `state.target` to apply it
    /// (design §5). Best-effort — network/parse failures are logged, never fatal.
    fn run_update_check(&self) {
        let manifest = match manifest::fetch(self.cfg) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "update check: manifest fetch failed");
                self.note_error(&format!("update check: {e}"));
                return;
            }
        };
        if manifest.name != self.cfg.global.app {
            tracing::warn!(
                manifest = manifest.name,
                app = self.cfg.global.app,
                "update check: manifest name mismatch"
            );
            return;
        }
        // No `floor`: `policy_action`'s `is_newer` gate (below, against the running
        // version) already refuses a downgrade, so resolve the raw latest here.
        let latest = match manifest::resolve_target(
            &manifest,
            &self.cfg.update.channel,
            self.cfg.update.pin.as_deref(),
            Some("latest"),
            None,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "update check: cannot resolve channel latest");
                return;
            }
        };

        let action = policy_action(
            self.cfg.update.policy,
            self.cfg.update.pin.is_some(),
            &latest,
            &self.target.version,
        );
        // P2-11: never auto-RE-apply a version whose last rollout entry is `bad`
        // (it was rolled back) — downgrade to advertise-only. Lenient read: with
        // no readable history there is nothing known-bad.
        let prior = state::read_lenient(&self.cfg.global.data_dir.join("state.json"));
        let action = gate_policy_action(action, &prior.unwrap_or_default());
        // `available` advertises a newer version (cleared when up to date);
        // `target` is only set for `auto` and must never clobber an app request.
        let (available, target) = match &action {
            PolicyAction::Idle => (None, None),
            PolicyAction::Advertise(v) => (Some(v.clone()), None),
            PolicyAction::Apply(v) => (Some(v.clone()), Some(v.clone())),
        };
        let now = now_timestamp();
        let channel = self.cfg.update.channel.clone();
        self.mutate_state(|st| {
            st.last_check = Some(now);
            st.channel = Some(channel);
            st.available = available;
            if let Some(target) = target {
                st.target = Some(target);
            }
        });
        match action {
            PolicyAction::Idle => {
                tracing::debug!(
                    latest,
                    current = self.target.version,
                    "update check: up to date"
                );
            }
            PolicyAction::Advertise(v) => {
                tracing::info!(available = v, "update check: newer version available");
            }
            PolicyAction::Apply(v) => {
                tracing::info!(target = v, "update check: auto-applying newer version");
            }
        }
    }

    /// Apply an update `target` via the stop-start hot-update (design §5): ensure
    /// it is installed, graceful-stop the old child, atomically switch `current`,
    /// start the new child, and enter the readiness/rollback observation window.
    /// Install/locate failures keep the current version running; a switch/spawn
    /// failure for the NEW version (the old child is already stopped by then) rolls
    /// back to the version that was running — no failure here exits PID 1 (design
    /// §8). Returns `Some(Outcome)` only when a nested failure dead-ends into the
    /// `restart=off` mirror exit.
    fn apply_target(&mut self, version: &str) -> Option<Outcome> {
        if version == self.target.version {
            self.clear_target(); // already on it — just drop the request
            return None;
        }
        tracing::info!(
            from = self.target.version,
            to = version,
            "applying update target"
        );

        if let Err(e) = self.ensure_installed(version) {
            tracing::error!(error = %e, version, "cannot install update target; staying on current");
            self.note_error(&format!("install {version}: {e}"));
            self.clear_target();
            return None;
        }
        let new_target = match locate(self.cfg, version) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, version, "update target not usable; staying on current");
                self.note_error(&format!("locate {version}: {e}"));
                self.clear_target();
                return None;
            }
        };

        let fallback = self.target.version.clone();
        self.set_status(Status::Updating);
        if self.child.is_some() {
            self.stop_child();
        }
        if let Err(e) = install::switch_current(self.cfg, version) {
            // The old child is already stopped, but `current` still points at its
            // version (the switch failed) — restart it rather than propagating.
            tracing::error!(error = %e, version, "cannot switch to update target; restarting current");
            self.note_error(&format!("switch {version}: {e}"));
            self.clear_target();
            self.phase = Phase::Run;
            return self.spawn_supervised();
        }
        self.target = new_target;
        self.restart_count = 0;
        self.restart_at = None;
        // Enter the observation before spawning so a start failure rolls back
        // through the same single-strike machinery as a post-start crash.
        self.phase = Phase::Observe(Observe {
            applied: version.to_owned(),
            fallback: Some(fallback),
            deadline: Instant::now() + Duration::from_secs(self.cfg.supervise.ready_timeout),
        });
        // Consume the `target` request + stale `ready` BEFORE the spawn (P2-14):
        // a fast child can write its `{instance}-0` serving token the moment it
        // exists, and a post-spawn clear would clobber it (spurious
        // readiness-timeout rollback). The pid follows in a second, smaller write.
        self.write_pre_observe_state();
        match self.spawn_child() {
            Ok(pid) => {
                self.record_child_pid(pid);
                None
            }
            // The new version cannot even start (lost exec bit, bad interpreter,
            // wrong-arch binary): single-strike rollback to the fallback. The
            // `target` request + stale `ready` were already consumed by the
            // pre-spawn write, so the bad version is not immediately re-applied.
            Err(e) => {
                tracing::error!(error = %e, version, "update target failed to start; rolling back");
                self.note_error(&format!("start {version}: {e}"));
                self.observe_failed("failed to start", None)
            }
        }
    }

    /// Ensure `version` is installed, downloading + verifying + installing it (via
    /// the audited [`crate::install`] path) when absent. No-op if already present.
    fn ensure_installed(&self, version: &str) -> Result<()> {
        if version_installed(self.cfg, version) {
            return Ok(());
        }
        tracing::info!(version, "update target not installed; downloading");
        let manifest = manifest::fetch(self.cfg)?;
        if manifest.name != self.cfg.global.app {
            return Err(Error::Manifest(format!(
                "manifest name {:?} does not match configured app {:?}",
                manifest.name, self.cfg.global.app
            )));
        }
        let entry = manifest::version_entry(&manifest, version)?;
        let asset = manifest::select_asset(entry, required_asset(self.cfg)?)?;
        let (artifact, sha256) =
            download::fetch_artifact(self.cfg, asset, version, &manifest::allowed_hosts(self.cfg))?;
        install::install(self.cfg, version, asset, &artifact, &sha256)
    }

    /// Stage an update target and hand the cut-over timing to the app (design §8,
    /// `readiness=state`): install it (the old child keeps serving), prompt the
    /// running app with `state.ready = {instance}-1`, and enter [`Phase::Prepare`].
    /// `target` stays in `state.json` (consumed at the actual cut-over, so a crash
    /// here still applies it via update-on-exit). A failed install keeps the current
    /// version running and drops the request.
    fn begin_prepare(&mut self, version: &str) {
        if let Err(e) = self.ensure_installed(version) {
            tracing::error!(error = %e, version, "cannot stage update target; staying on current");
            self.note_error(&format!("install {version}: {e}"));
            self.clear_target();
            return;
        }
        let prompt = ready_token(&self.instance, READY_PREPARE);
        tracing::info!(
            version,
            instance = self.instance,
            "update target staged — prompting app to prepare for cut-over"
        );
        self.mutate_state(|st| {
            st.status = Some(Status::Updating);
            st.ready = Some(prompt);
        });
        self.phase = Phase::Prepare(Prepare {
            target: version.to_owned(),
            started: Instant::now(),
        });
    }

    /// One prepare tick: cut over to the staged target once the app acks it is
    /// prepared (`state.ready = {instance}-2`). By default there is no timeout —
    /// the app sets the pace, and the old child stays supervised while it prepares
    /// (design §8); a configured `prepare_timeout` forces the cut-over so a
    /// never-acking app cannot wedge the staged update (P2-13). Returns
    /// `Some(Outcome)` only when the cut-over dead-ends into a mirror exit.
    fn poll_prepare(&mut self) -> Option<Outcome> {
        let Phase::Prepare(prep) = &self.phase else {
            return None;
        };
        let version = prep.target.clone();
        let elapsed = prep.started.elapsed();
        let acked = self.prepare_ready();
        if !prepare_cutover_due(acked, elapsed, self.cfg.supervise.prepare_timeout) {
            return None;
        }
        if acked {
            tracing::info!(
                version,
                "app acked prepared — cutting over to staged target"
            );
        } else {
            tracing::warn!(
                version,
                timeout = self.cfg.supervise.prepare_timeout,
                "app did not ack prepare within prepare_timeout; forcing cut-over"
            );
        }
        let outcome = self.apply_target(&version);
        if matches!(self.phase, Phase::Prepare(_)) {
            // The cut-over failed before any phase transition (install/locate):
            // apply_target already dropped the `target` request — also abandon
            // the prepare, or lode would keep re-attempting a cut-over that can
            // never complete on every tick (spec addendum).
            tracing::warn!(
                version,
                "staged cut-over failed; abandoning prepare and resuming normal supervision"
            );
            self.abandon_prepare();
        }
        outcome
    }

    /// Abandon a staged prepare: return to normal supervision and clear the
    /// handshake token (the `-1` prompt — or the app's `-2` ack) from
    /// `state.ready` so it cannot confuse the next spawn (P2-13 / addendum).
    /// The old child keeps running; status returns to `running`.
    fn abandon_prepare(&mut self) {
        self.phase = Phase::Run;
        self.mutate_state(|st| {
            st.status = Some(Status::Running);
            st.ready = None;
        });
    }

    /// Has the running app acked the staged-update prompt for *this* spawn
    /// (`state.ready == {instance}-2`)? A dead child never has (§8).
    fn prepare_ready(&self) -> bool {
        if self.child.is_none() {
            return false;
        }
        let path = self.cfg.global.data_dir.join("state.json");
        let ready = state::read_lenient(&path).and_then(|st| st.ready);
        ready.as_deref() == Some(ready_token(&self.instance, READY_GO).as_str())
    }

    /// One observation tick on a freshly-applied target: commit it as `last_good`
    /// once ready, or roll back on a readiness timeout / crash threshold (§5/§8).
    fn poll_observe(&mut self) -> Option<Outcome> {
        if !matches!(self.phase, Phase::Observe(_)) {
            return None;
        }
        let ready = self.observe_ready();
        let timed_out = self.observe_timed_out();
        match observe_decision(ready, timed_out) {
            ObserveOutcome::Pending => None,
            ObserveOutcome::Commit => {
                self.commit_update();
                None
            }
            ObserveOutcome::Rollback => self.observe_failed("readiness timeout", None),
        }
    }

    /// Has the observed child signalled readiness for this spawn (design §8)?
    /// A dead child (between crash and backoff respawn) is never ready.
    fn observe_ready(&self) -> bool {
        if self.child.is_none() {
            return false;
        }
        let ready_field = match self.cfg.supervise.readiness {
            Readiness::None => None,
            Readiness::State => {
                let path = self.cfg.global.data_dir.join("state.json");
                state::read_lenient(&path).and_then(|st| st.ready)
            }
        };
        readiness_met(
            self.cfg.supervise.readiness,
            ready_field.as_deref(),
            &self.instance,
            self.spawn_at.elapsed(),
            Duration::from_secs(self.cfg.supervise.health_grace),
        )
    }

    /// Has the `readiness=state` handshake exceeded `ready_timeout`? (No timeout
    /// applies in `none` mode — it resolves via grace-survival or the crash count.)
    fn observe_timed_out(&self) -> bool {
        matches!(self.cfg.supervise.readiness, Readiness::State)
            && match &self.phase {
                Phase::Observe(obs) => Instant::now() >= obs.deadline,
                // No timeout while preparing — the app paces the cut-over (§8).
                Phase::Run | Phase::Prepare(_) => false,
            }
    }

    /// Commit the observed target: mark it `running` + `last_good`, append a `good`
    /// history entry, prune old versions, and return to the `Run` phase.
    fn commit_update(&mut self) {
        let applied = match &self.phase {
            Phase::Observe(obs) => obs.applied.clone(),
            Phase::Run | Phase::Prepare(_) => return,
        };
        tracing::info!(version = applied, "update ready — committing as last_good");
        self.restart_count = 0;
        let at = now_timestamp();
        self.mutate_state(|st| {
            st.status = Some(Status::Running);
            st.current = Some(applied.clone());
            st.last_good = Some(applied.clone());
            st.available = None;
            st.last_error = None;
            push_history(&mut st.history, &applied, HistoryResult::Good, at);
        });
        if let Err(e) = install::prune(self.cfg, Some(&applied), Some(&applied)) {
            tracing::warn!(error = %e, "prune after update failed");
        }
        self.phase = Phase::Run;
    }

    /// A failed observation (crash within grace, a readiness timeout, or a start
    /// failure). Roll back to the fallback and observe it; if there is no fallback
    /// — we were already observing `last_good` — there is nothing good left to run,
    /// so lode PAUSES (keep-alive: stays alive, awaiting recovery) rather than
    /// exiting, or mirror-exits under `off`. `status` carries the child's exit
    /// status for a crash, `None` otherwise.
    fn observe_failed(&mut self, reason: &str, status: Option<WaitStatus>) -> Option<Outcome> {
        let (applied, fallback) = match &self.phase {
            Phase::Observe(obs) => (obs.applied.clone(), obs.fallback.clone()),
            Phase::Run | Phase::Prepare(_) => return None,
        };
        if let Some(fallback) = fallback {
            return self.rollback_to(&applied, &fallback, reason);
        }

        // No further fallback: the rollback target (last_good) itself failed.
        tracing::error!(version = applied, reason, "rollback target failed");
        if self.child.is_some() {
            self.stop_child();
        }
        self.phase = Phase::Run;
        let code = status.map_or(1, exit_code_from);
        let code = if code == 0 { 1 } else { code };
        let at = now_timestamp();
        self.mutate_state(|st| {
            st.last_error = Some(format!("rollback target {applied} failed: {reason}"));
            push_history(&mut st.history, &applied, HistoryResult::Bad, at);
        });
        self.pause_or_exit(code)
    }

    /// Roll back the failed `applied` version to `fallback` (the version it
    /// replaced): stop the failed child, switch `current` back, spawn the fallback
    /// and OBSERVE it (a fresh activation that must itself survive its grace),
    /// appending a `bad` history entry for `applied` (design §5). Every failure
    /// here stays inside the keep-alive machinery — a fallback that cannot be
    /// switched to / located runs the failed version as a best effort, and a
    /// fallback that cannot be spawned goes through the bounded-backoff
    /// retry-then-pause path — never out of the process (design §8). Returns
    /// `Some(Outcome)` only for the `restart=off` mirror exit.
    fn rollback_to(&mut self, applied: &str, fallback: &str, reason: &str) -> Option<Outcome> {
        tracing::warn!(
            failed = applied,
            fallback,
            reason,
            "update failed — rolling back"
        );
        self.set_status(Status::RollingBack);
        if self.child.is_some() {
            self.stop_child();
        }
        self.restart_count = 0;
        self.restart_at = None;

        if !version_installed(self.cfg, fallback) {
            // The known-good version is gone — keep the failed version running as a
            // best effort rather than leaving nothing supervised.
            tracing::error!(fallback, "rollback target is not installed");
            self.note_error(&format!("rollback target {fallback} not installed"));
            self.phase = Phase::Run;
            return self.spawn_supervised();
        }

        // Record the strike against `applied` up front, so the history survives
        // whichever spawn path the rollback takes from here.
        let at = now_timestamp();
        self.mutate_state(|st| push_history(&mut st.history, applied, HistoryResult::Bad, at));

        if let Err(e) = install::switch_current(self.cfg, fallback) {
            // Cannot point `current` back — keep the failed version supervised
            // (best effort) rather than leaving nothing running.
            tracing::error!(error = %e, fallback, "cannot switch back to rollback target");
            self.note_error(&format!("switch {fallback}: {e}"));
            self.phase = Phase::Run;
            return self.spawn_supervised();
        }
        match locate(self.cfg, fallback) {
            Ok(t) => self.target = t,
            Err(e) => {
                tracing::error!(error = %e, fallback, "rollback target not usable");
                self.note_error(&format!("locate {fallback}: {e}"));
                self.phase = Phase::Run;
                return self.spawn_supervised();
            }
        }
        let pid = match self.spawn_child() {
            Ok(pid) => pid,
            Err(e) => {
                // The fallback cannot start either: `target`/`current` already point
                // at it, so hand off to the bounded-backoff retry-then-pause path.
                self.phase = Phase::Run;
                return self.on_spawn_failure(&e);
            }
        };
        let pid_u32 = u32::try_from(pid.as_raw()).ok();
        self.mutate_state(|st| {
            st.status = Some(Status::Updating);
            st.current = Some(fallback.to_owned());
            st.pid = pid_u32;
        });
        // Observe the rollback target; with no further fallback, a failure pauses.
        self.phase = Phase::Observe(Observe {
            applied: fallback.to_owned(),
            fallback: None,
            deadline: Instant::now() + Duration::from_secs(self.cfg.supervise.ready_timeout),
        });
        None
    }
}

// --- setup helpers ---

/// Acquire the single-instance PID lock (RAII; released on drop).
fn lock_acquire(cfg: &Config) -> Result<crate::lock::LockGuard> {
    crate::lock::acquire(&cfg.global.data_dir, &cfg.global.app)
}

/// Become a child subreaper so re-parented grandchildren are reaped by us (PID 1
/// init duty). Best-effort: a failure is logged, not fatal.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_subreaper() {
    if let Err(e) = nix::sys::prctl::set_child_subreaper(true) {
        tracing::warn!(error = %e, "could not set child subreaper");
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn set_subreaper() {}

/// Validate a `state.pid` for orphan reaping: `None` when it does not fit an
/// i32 or is <= 1 — pid 0/1 is never a real app child, and signalling either
/// would hit lode's own process group / broadcast system-wide as PID 1 (R2-1).
fn orphan_pid(st_pid: Option<u32>) -> Option<Pid> {
    let raw = i32::try_from(st_pid?).ok()?;
    (raw > 1).then(|| Pid::from_raw(raw))
}

/// Startup cleanup (design §5): terminate an orphaned app child left by a crashed
/// lode (from `state.pid`), then GC interrupted downloads / staging.
fn startup_cleanup(cfg: &Config) -> Result<()> {
    // Lenient: boot path — a corrupt state.json means no orphan to reap, not a
    // dead supervisor.
    let state_path = cfg.global.data_dir.join("state.json");
    if let Some(st) = state::read_lenient(&state_path)
        && let Some(st_pid) = st.pid
    {
        if let Some(pid) = orphan_pid(Some(st_pid)) {
            if process_alive(pid) {
                tracing::warn!(
                    pid = pid.as_raw(),
                    "terminating orphaned app child from a previous lode"
                );
                terminate_external(pid, Duration::from_secs(cfg.supervise.stop_timeout));
            }
        } else {
            // pid 0/1 (or out of i32 range) is never a real app child — probing
            // it "alive" and signalling would hit lode's own process group or,
            // as PID 1, broadcast across the container (R2-1).
            tracing::warn!(
                pid = st_pid,
                "implausible pid in state.json; skipping orphan reap"
            );
        }
    }
    clear_stale_ready(&state_path)?;
    install::gc(cfg)
}

/// Drop a stale `state.ready` left by a previous lode run so it can never be
/// mistaken for this run's first spawn — defence-in-depth alongside the
/// per-spawn random `LODE_INSTANCE` token ([`nanoid`]). All other state fields
/// (`current` / `last_good` / …) are preserved.
fn clear_stale_ready(state_path: &Path) -> Result<()> {
    if let Some(mut st) = state::read_lenient(state_path)
        && st.ready.is_some()
    {
        st.ready = None;
        state::write(state_path, &st)?;
    }
    Ok(())
}

/// A per-spawn random token (16 lowercase base-36 chars, dash-free) forming the
/// unique half of `LODE_INSTANCE` (`{pid}-{nanoid}`). A fresh draw per spawn makes
/// every readiness-handshake id unique — across lode restarts and even if the OS
/// reuses this pid — so a stale `state.ready` can never false-match a fresh spawn.
/// Dash-free so the trailing `-{phase}` suffix stays unambiguous (§8). Degrades to
/// a fixed token (pid still disambiguates) only if the OS RNG is unavailable.
fn nanoid() -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        return "0".repeat(bytes.len());
    }
    bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- signal classification ---

    #[test]
    fn classify_termination_signals() {
        let fwd = default_forward();
        for sig in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGQUIT] {
            assert_eq!(classify(sig, None, &fwd), Action::Terminate);
        }
    }

    #[test]
    fn classify_forward_and_ignore() {
        let fwd = default_forward();
        assert_eq!(classify(Signal::SIGHUP, None, &fwd), Action::Forward);
        assert_eq!(classify(Signal::SIGCONT, None, &fwd), Action::Forward);
        // SIGPIPE is in neither set.
        assert_eq!(classify(Signal::SIGPIPE, None, &fwd), Action::Ignore);
    }

    #[test]
    fn classify_restart_signal_wins_over_forward() {
        let fwd = default_forward();
        // SIGUSR2 is in the default forward set, but a configured restart signal
        // takes precedence and is never forwarded.
        assert_eq!(
            classify(Signal::SIGUSR2, Some(Signal::SIGUSR2), &fwd),
            Action::Restart
        );
        assert_eq!(
            classify(Signal::SIGHUP, Some(Signal::SIGUSR2), &fwd),
            Action::Forward
        );
    }

    #[test]
    fn forward_signals_default_and_parsed() {
        assert_eq!(forward_signals(&[]).len(), 6);
        assert_eq!(
            forward_signals(&["SIGHUP".to_owned(), "usr1".to_owned()]),
            vec![Signal::SIGHUP, Signal::SIGUSR1]
        );
        // Unparsable names are dropped, not fatal.
        assert_eq!(
            forward_signals(&["bogus".to_owned(), "HUP".to_owned()]),
            vec![Signal::SIGHUP]
        );
    }

    #[test]
    fn parse_signal_accepts_both_forms() {
        assert_eq!(parse_signal("SIGHUP"), Some(Signal::SIGHUP));
        assert_eq!(parse_signal("hup"), Some(Signal::SIGHUP));
        assert_eq!(parse_signal(" Sigusr2 "), Some(Signal::SIGUSR2));
        assert_eq!(parse_signal("nonsense"), None);
    }

    #[test]
    fn forbidden_signals_detected() {
        assert!(is_forbidden(Signal::SIGKILL));
        assert!(is_forbidden(Signal::SIGSTOP));
        assert!(!is_forbidden(Signal::SIGTERM));
    }

    // --- backoff schedule ---

    #[test]
    fn backoff_doubles_then_caps() {
        let base = 1;
        let max = 30;
        assert_eq!(backoff_delay(0, base, max), Duration::from_secs(1));
        assert_eq!(backoff_delay(1, base, max), Duration::from_secs(2));
        assert_eq!(backoff_delay(2, base, max), Duration::from_secs(4));
        assert_eq!(backoff_delay(5, base, max), Duration::from_secs(30)); // 32 -> cap
        // A huge attempt saturates to the cap instead of overflowing.
        assert_eq!(backoff_delay(99, base, max), Duration::from_secs(30));
    }

    // --- exit codes ---

    #[test]
    fn exit_code_from_status() {
        let pid = Pid::from_raw(1234);
        assert_eq!(exit_code_from(WaitStatus::Exited(pid, 7)), 7);
        assert_eq!(
            exit_code_from(WaitStatus::Signaled(pid, Signal::SIGTERM, false)),
            128 + 15
        );
        assert_eq!(exit_code_from(WaitStatus::StillAlive), 0);
    }

    // --- env stripping + injection ---

    #[test]
    fn child_env_strips_lode_and_injects() {
        let host = vec![
            ("LODE_MANIFEST".to_owned(), "https://x".to_owned()),
            ("LODE_DATA_DIR".to_owned(), "/old".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("HOME".to_owned(), "/root".to_owned()),
        ];
        let env = child_env(
            host,
            &BTreeMap::new(),
            "1.2.3",
            Path::new("/data"),
            "inst-9",
            None,
        );
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();

        // All config LODE_* are stripped from the inherited set...
        assert!(!map.contains_key("LODE_MANIFEST"));
        // ...and host env passes through.
        assert_eq!(map.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(map.get("HOME").map(String::as_str), Some("/root"));
        // Introspection vars are injected (LODE_DATA_DIR re-set to the resolved dir).
        assert_eq!(
            map.get("LODE_ACTIVE_VERSION").map(String::as_str),
            Some("1.2.3")
        );
        assert_eq!(map.get("LODE_DATA_DIR").map(String::as_str), Some("/data"));
        assert_eq!(map.get("LODE_INSTANCE").map(String::as_str), Some("inst-9"));
    }

    #[test]
    fn child_env_defined_are_defaults_host_wins() {
        let host = vec![
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("NODE_ENV".to_owned(), "development".to_owned()),
        ];
        let defined: BTreeMap<String, String> = [
            ("NODE_ENV".to_owned(), "production".to_owned()), // host has it → host wins
            ("APP_FLAG".to_owned(), "on".to_owned()),         // host lacks it → default applied
            ("LODE_DATA_DIR".to_owned(), "/hijack".to_owned()), // lode's var still wins below
        ]
        .into_iter()
        .collect();
        let env = child_env(host, &defined, "1.0.0", Path::new("/data"), "i", None);

        // Exactly one entry per key — defaults fill gaps, they don't duplicate.
        assert_eq!(env.iter().filter(|(k, _)| k == "NODE_ENV").count(), 1);
        assert_eq!(env.iter().filter(|(k, _)| k == "LODE_DATA_DIR").count(), 1);

        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        // Inherited host env wins over a same-named [env] default (12-factor `-e`).
        assert_eq!(map.get("NODE_ENV").map(String::as_str), Some("development"));
        // A [env] key the host lacks is applied as the default.
        assert_eq!(map.get("APP_FLAG").map(String::as_str), Some("on"));
        // lode's injected vars still win over any [env] of the same name.
        assert_eq!(map.get("LODE_DATA_DIR").map(String::as_str), Some("/data"));
    }

    #[test]
    fn child_env_host_path_wins_over_defined_then_runtime_prepends() {
        // A host PATH beats a [env] PATH default; the runtime dir still prepends.
        let host = vec![("PATH".to_owned(), "/usr/bin".to_owned())];
        let mut defined = BTreeMap::new();
        defined.insert("PATH".to_owned(), "/opt/bin".to_owned()); // ignored: host has PATH
        let env = child_env(
            host,
            &defined,
            "1.0.0",
            Path::new("/data"),
            "i",
            Some(Path::new("/rt")),
        );
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        assert_eq!(path.as_deref(), Some("/rt:/usr/bin"));
    }

    #[test]
    fn child_env_prepends_runtime_to_path() {
        let host = vec![("PATH".to_owned(), "/usr/bin".to_owned())];
        let env = child_env(
            host,
            &BTreeMap::new(),
            "1.0.0",
            Path::new("/data"),
            "i",
            Some(Path::new("/data/runtime")),
        );
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        assert_eq!(path.as_deref(), Some("/data/runtime:/usr/bin"));
    }

    #[test]
    fn child_env_prepends_runtime_to_defined_path() {
        // When the host has no PATH, the [env] default is used — and still extended
        // by the runtime prepend.
        let mut defined = BTreeMap::new();
        defined.insert("PATH".to_owned(), "/opt/bin".to_owned());
        let env = child_env(
            Vec::new(),
            &defined,
            "1.0.0",
            Path::new("/data"),
            "i",
            Some(Path::new("/rt")),
        );
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        assert_eq!(path.as_deref(), Some("/rt:/opt/bin"));
    }

    #[test]
    fn child_env_creates_path_when_absent() {
        let env = child_env(
            Vec::new(),
            &BTreeMap::new(),
            "1.0.0",
            Path::new("/data"),
            "i",
            Some(Path::new("/rt")),
        );
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        assert_eq!(path.as_deref(), Some("/rt"));
    }

    // --- launch-command resolution + argv build + {dir} expansion ---

    #[test]
    fn effective_command_override_wins_then_config_then_error() {
        // Manifest override (marker) wins over the operator's [command] value.
        assert_eq!(
            effective_command(Some("./pub run"), Some("./op"), "run").unwrap(),
            "./pub run"
        );
        // No override → the configured value.
        assert_eq!(
            effective_command(None, Some("./op"), "run").unwrap(),
            "./op"
        );
        // Blank strings count as unset on both sides.
        assert_eq!(
            effective_command(Some("  "), Some("./op"), "run").unwrap(),
            "./op"
        );
        // Neither side → the clear, actionable hard error.
        let err = effective_command(None, Some("   "), "run").unwrap_err();
        assert!(err.to_string().contains("no run command"), "got: {err}");
        assert!(err.to_string().contains("[command].run"), "got: {err}");
        let err = effective_command(None, None, "exec").unwrap_err();
        assert!(err.to_string().contains("no exec command"), "got: {err}");
    }

    #[test]
    fn run_argv_is_literal_with_dir_expansion() {
        // A literal command is whitespace-split verbatim — nothing is appended.
        assert_eq!(
            build_run_argv("bun run app.js", "/v").unwrap(),
            vec!["bun", "run", "app.js"]
        );
        // {dir} expands to the version dir; an absolute program is untouched.
        assert_eq!(
            build_run_argv("{dir}/app serve --dir {dir}", "/v").unwrap(),
            vec!["/v/app", "serve", "--dir", "/v"]
        );
        // Whitespace-only commands are caught upstream by effective_command, but
        // the builder still refuses an empty argv.
        assert!(build_run_argv("  ", "/v").is_err());
    }

    #[test]
    fn exec_argv_appends_args_verbatim() {
        assert_eq!(
            build_exec_argv("bun", "/v", &["run".to_owned(), "db:init".to_owned()]).unwrap(),
            vec!["bun", "run", "db:init"]
        );
        assert_eq!(
            build_exec_argv("{dir}/app", "/v", &["--flag".to_owned()]).unwrap(),
            vec!["/v/app", "--flag"]
        );
    }

    #[test]
    fn resolve_program_absolutizes_version_dir_files_only() {
        let dir = std::env::temp_dir().join(format!("lode-rp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app"), b"#!/bin/sh\n").unwrap();
        let dir_s = dir.to_string_lossy().into_owned();

        // "./app" names a version-dir file → absolutized (works under any workdir).
        let argv = build_run_argv("./app serve", &dir_s).unwrap();
        assert_eq!(argv[0], dir.join("app").to_string_lossy());
        assert_eq!(argv[1], "serve");
        // A bare name that is NOT a version-dir file stays a PATH lookup…
        assert_eq!(build_run_argv("bun run app.ts", &dir_s).unwrap()[0], "bun");
        // …and exec resolves the program but never its args.
        let argv = build_exec_argv("./app", &dir_s, &["./app".to_owned()]).unwrap();
        assert_eq!(argv[0], dir.join("app").to_string_lossy());
        assert_eq!(argv[1], "./app");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- runtime decision + format inference ---

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
            use std::os::unix::fs::PermissionsExt as _;
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
        use std::os::unix::fs::PermissionsExt as _;

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

    // --- real-child process helpers (specific-pid; CI-safe) ---

    #[cfg(unix)]
    #[test]
    fn graceful_stop_terminates_a_sleeping_child() {
        // Spawn a long sleep; SIGTERM ends it immediately, well within the timeout.
        let pid = spawn_process(
            &["sleep".to_owned(), "30".to_owned()],
            Path::new("/"),
            &[("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
        )
        .unwrap();
        let status = graceful_stop(pid, Duration::from_secs(5));
        assert!(matches!(
            status,
            Some(WaitStatus::Signaled(_, Signal::SIGTERM, _))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn graceful_stop_reaps_already_exited_child() {
        // A child that exits on its own becomes a zombie until reaped; graceful_stop
        // signals (a no-op for a zombie) then reaps, yielding its real exit code.
        let pid = spawn_process(
            &["sh".to_owned(), "-c".to_owned(), "exit 4".to_owned()],
            Path::new("/"),
            &[("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
        )
        .unwrap();
        // Give it a moment to exit.
        std::thread::sleep(Duration::from_millis(100));
        let status = graceful_stop(pid, Duration::from_secs(5));
        assert!(matches!(status, Some(WaitStatus::Exited(_, 4))));
    }

    #[cfg(unix)]
    #[test]
    fn graceful_stop_terminates_the_whole_process_group() {
        let dir = std::env::temp_dir().join(format!("lode-pgroup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("grandchild.pid");
        // The child backgrounds a grandchild (same process group — fork-model
        // worker stand-in), records its pid (write+rename so the read is never
        // torn), then execs into a long sleep. Stopping must take down BOTH.
        let script = format!(
            "sleep 30 & echo $! > {pf}.tmp && mv {pf}.tmp {pf}; exec sleep 30",
            pf = pidfile.display()
        );
        let pid = spawn_process(
            &["sh".to_owned(), "-c".to_owned(), script],
            Path::new("/"),
            &[("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
        )
        .unwrap();

        // Wait for the grandchild pid to land on disk.
        let deadline = Instant::now() + Duration::from_secs(5);
        let grandchild = loop {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && let Ok(raw) = text.trim().parse::<i32>()
            {
                break Pid::from_raw(raw);
            }
            assert!(Instant::now() < deadline, "grandchild pid never appeared");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(process_alive(grandchild));

        let status = graceful_stop(pid, Duration::from_secs(5));
        assert!(matches!(
            status,
            Some(WaitStatus::Signaled(_, Signal::SIGTERM, _))
        ));

        // The grandchild shared the child's group, so the group TERM reached it
        // too; it re-parents to init and is reaped there — poll for it to vanish.
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_alive(grandchild) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !process_alive(grandchild),
            "grandchild must die with the process group"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- R2-1: pid <= 1 must never become a kill() target (group or bare) ---

    #[test]
    fn group_target_refuses_pid_zero_and_one() {
        // Negating 0 / 1 would signal lode's own group / broadcast system-wide.
        assert_eq!(group_target(Pid::from_raw(0)), None);
        assert_eq!(group_target(Pid::from_raw(1)), None);
        assert_eq!(group_target(Pid::from_raw(-1)), None);
        assert_eq!(
            group_target(Pid::from_raw(1234)),
            Some(Pid::from_raw(-1234))
        );
    }

    #[cfg(unix)]
    #[test]
    fn signal_child_refuses_pid_zero_and_one() {
        // SIGCONT keeps the assertion harmless even against unguarded code,
        // where pid 0 would otherwise signal the test runner's own group.
        assert_eq!(
            signal_child(Pid::from_raw(0), Signal::SIGCONT),
            Err(Errno::ESRCH)
        );
        assert_eq!(
            signal_child(Pid::from_raw(1), Signal::SIGCONT),
            Err(Errno::ESRCH)
        );
    }

    #[test]
    fn orphan_pid_rejects_implausible_state_pids() {
        assert_eq!(orphan_pid(None), None);
        assert_eq!(orphan_pid(Some(0)), None);
        assert_eq!(orphan_pid(Some(1)), None);
        assert_eq!(orphan_pid(Some(u32::MAX)), None); // does not fit an i32
        assert_eq!(orphan_pid(Some(1234)), Some(Pid::from_raw(1234)));
    }

    // --- C2: semver newer-than comparison ---

    #[test]
    fn is_newer_compares_semver_then_falls_back() {
        assert!(is_newer("1.5.0", "1.4.2"));
        assert!(!is_newer("1.4.2", "1.5.0"));
        // Equal precedence is never newer (no auto-apply loop).
        assert!(!is_newer("1.4.2", "1.4.2"));
        // Semver beats lexicographic: 1.10.0 > 1.9.0.
        assert!(is_newer("1.10.0", "1.9.0"));
        // Non-semver ids: any *different* id counts as newer; identical does not.
        assert!(is_newer("nightly-2", "nightly-1"));
        assert!(!is_newer("nightly-1", "nightly-1"));
    }

    // --- C2: policy decision (off / check / auto + pin) ---

    #[test]
    fn policy_action_off_and_pinned_are_idle() {
        // `off` never acts; a pin forces idle regardless of policy.
        assert_eq!(
            policy_action(Policy::Off, false, "2.0.0", "1.0.0"),
            PolicyAction::Idle
        );
        assert_eq!(
            policy_action(Policy::Auto, true, "2.0.0", "1.0.0"),
            PolicyAction::Idle
        );
        assert_eq!(
            policy_action(Policy::Check, true, "2.0.0", "1.0.0"),
            PolicyAction::Idle
        );
    }

    #[test]
    fn policy_action_check_advertises_and_auto_applies() {
        assert_eq!(
            policy_action(Policy::Check, false, "2.0.0", "1.0.0"),
            PolicyAction::Advertise("2.0.0".to_owned())
        );
        assert_eq!(
            policy_action(Policy::Auto, false, "2.0.0", "1.0.0"),
            PolicyAction::Apply("2.0.0".to_owned())
        );
    }

    #[test]
    fn policy_action_idle_when_not_newer() {
        // Already current → nothing to advertise/apply, for both check and auto.
        assert_eq!(
            policy_action(Policy::Check, false, "1.0.0", "1.0.0"),
            PolicyAction::Idle
        );
        assert_eq!(
            policy_action(Policy::Auto, false, "1.0.0", "2.0.0"),
            PolicyAction::Idle
        );
    }

    // --- P2-11: known-bad version memory (policy=auto re-apply gate) ---

    fn hist(version: &str, result: HistoryResult) -> HistoryEntry {
        HistoryEntry {
            version: version.to_owned(),
            at: "t".to_owned(),
            result,
        }
    }

    fn state_with_history(entries: Vec<HistoryEntry>) -> State {
        State {
            history: entries,
            ..State::default()
        }
    }

    #[test]
    fn version_known_bad_tracks_latest_verdict() {
        // No history → nothing is known-bad.
        assert!(!version_known_bad(&State::default(), "2.0.0"));
        // A `bad` entry (rollback strike) marks the version.
        let st = state_with_history(vec![hist("2.0.0", HistoryResult::Bad)]);
        assert!(version_known_bad(&st, "2.0.0"));
        // …but only THAT version.
        assert!(!version_known_bad(&st, "2.0.1"));
        // A later `good` for the same version clears the verdict.
        let st = state_with_history(vec![
            hist("2.0.0", HistoryResult::Bad),
            hist("2.0.0", HistoryResult::Good),
        ]);
        assert!(!version_known_bad(&st, "2.0.0"));
        // …and a later `bad` re-instates it (most recent entry wins).
        let st = state_with_history(vec![
            hist("2.0.0", HistoryResult::Good),
            hist("2.0.0", HistoryResult::Bad),
        ]);
        assert!(version_known_bad(&st, "2.0.0"));
    }

    #[test]
    fn gate_policy_action_downgrades_known_bad_apply_to_advertise() {
        let bad = state_with_history(vec![hist("2.0.0", HistoryResult::Bad)]);
        // Auto-apply of a known-bad version → advertise-only (no re-apply loop).
        assert_eq!(
            gate_policy_action(PolicyAction::Apply("2.0.0".to_owned()), &bad),
            PolicyAction::Advertise("2.0.0".to_owned())
        );
        // A version that was bad then good applies again.
        let recovered = state_with_history(vec![
            hist("2.0.0", HistoryResult::Bad),
            hist("2.0.0", HistoryResult::Good),
        ]);
        assert_eq!(
            gate_policy_action(PolicyAction::Apply("2.0.0".to_owned()), &recovered),
            PolicyAction::Apply("2.0.0".to_owned())
        );
        // Advertise/idle pass through untouched (advertising a bad version is
        // fine — the app decides whether to request it explicitly).
        assert_eq!(
            gate_policy_action(PolicyAction::Advertise("2.0.0".to_owned()), &bad),
            PolicyAction::Advertise("2.0.0".to_owned())
        );
        assert_eq!(
            gate_policy_action(PolicyAction::Idle, &bad),
            PolicyAction::Idle
        );
    }

    // --- P2-13: restart-nonce phase decision + prepare timeout ---

    #[test]
    fn nonce_action_acts_in_every_phase() {
        let prepare = Phase::Prepare(Prepare {
            target: "2.0.0".to_owned(),
            started: Instant::now(),
        });
        let observe = Phase::Observe(Observe {
            applied: "2.0.0".to_owned(),
            fallback: Some("1.0.0".to_owned()),
            deadline: Instant::now(),
        });
        // Paused wins regardless of phase: the bump is the recovery trigger.
        assert_eq!(nonce_action(true, &Phase::Run), NonceAction::Resume);
        assert_eq!(nonce_action(true, &prepare), NonceAction::Resume);
        // Running phases each get their own action — never swallowed.
        assert_eq!(nonce_action(false, &Phase::Run), NonceAction::Restart);
        assert_eq!(
            nonce_action(false, &prepare),
            NonceAction::AbandonPrepareAndRestart
        );
        assert_eq!(nonce_action(false, &observe), NonceAction::RestartObserved);
    }

    #[test]
    fn prepare_cutover_due_ack_wins_timeout_optional() {
        // The app's ack always cuts over, immediately.
        assert!(prepare_cutover_due(true, Duration::ZERO, 0));
        assert!(prepare_cutover_due(true, Duration::ZERO, 60));
        // timeout 0 = disabled: without an ack lode waits forever (app-paced §8).
        assert!(!prepare_cutover_due(false, Duration::from_hours(24), 0));
        // A configured timeout forces the cut-over once exceeded, not before.
        assert!(!prepare_cutover_due(false, Duration::from_secs(4), 5));
        assert!(prepare_cutover_due(false, Duration::from_secs(5), 5));
    }

    // --- C2: readiness gating (none vs state) ---

    #[test]
    fn readiness_none_waits_for_grace() {
        let grace = Duration::from_secs(15);
        assert!(!readiness_met(
            Readiness::None,
            None,
            "p-1",
            Duration::from_secs(5),
            grace
        ));
        assert!(readiness_met(
            Readiness::None,
            None,
            "p-1",
            Duration::from_secs(15),
            grace
        ));
        // `none` ignores any app-written ready value.
        assert!(readiness_met(
            Readiness::None,
            Some("anything"),
            "p-1",
            Duration::from_secs(20),
            grace
        ));
    }

    #[test]
    fn readiness_state_matches_this_instances_serving_signal() {
        let grace = Duration::from_secs(15);
        // Ready when the app reported *this* spawn serving — phased token `{instance}-0`
        // or (backward compat) the bare `{instance}`. Uptime is irrelevant in `state`.
        assert!(readiness_met(
            Readiness::State,
            Some("p-2-0"),
            "p-2",
            Duration::from_secs(0),
            grace
        ));
        assert!(readiness_met(
            Readiness::State,
            Some("p-2"),
            "p-2",
            Duration::from_secs(0),
            grace
        ));
        // A serving signal from a different instance does not count (phased or bare).
        assert!(!readiness_met(
            Readiness::State,
            Some("p-1-0"),
            "p-2",
            Duration::from_secs(99),
            grace
        ));
        assert!(!readiness_met(
            Readiness::State,
            Some("p-1"),
            "p-2",
            Duration::from_secs(99),
            grace
        ));
        // The prepare/go tokens are not "serving" — only `-0`/bare commit a spawn (§8).
        assert!(!readiness_met(
            Readiness::State,
            Some("p-2-2"),
            "p-2",
            Duration::from_secs(99),
            grace
        ));
        assert!(!readiness_met(
            Readiness::State,
            None,
            "p-2",
            Duration::from_secs(99),
            grace
        ));
    }

    #[test]
    fn ready_token_appends_the_phase_suffix() {
        assert_eq!(ready_token("12345-abc", READY_RUNNING), "12345-abc-0");
        assert_eq!(ready_token("12345-abc", READY_PREPARE), "12345-abc-1");
        assert_eq!(ready_token("12345-abc", READY_GO), "12345-abc-2");
    }

    #[test]
    fn nanoid_is_16_dashfree_lowercase_base36() {
        let id = nanoid();
        assert_eq!(id.len(), 16, "nanoid should be 16 chars: {id:?}");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()),
            "nanoid must be lowercase base-36 (dash-free): {id:?}"
        );
        // Two draws differ with overwhelming probability (fresh per spawn).
        assert_ne!(nanoid(), nanoid());
    }

    #[test]
    fn clear_stale_ready_drops_ready_keeps_the_rest() {
        let dir = std::env::temp_dir().join(format!("lode-clearready-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        // A prior run left `current` plus a now-stale readiness handshake.
        let st = State {
            current: Some("0.0.1".to_owned()),
            ready: Some("12345-deadbeef-2".to_owned()),
            ..State::default()
        };
        state::write(&path, &st).unwrap();

        clear_stale_ready(&path).unwrap();

        let back = state::read(&path).unwrap().unwrap();
        assert_eq!(back.ready, None, "stale ready must be cleared");
        assert_eq!(
            back.current.as_deref(),
            Some("0.0.1"),
            "other fields must be preserved"
        );

        // Idempotent: a second pass on already-clear state succeeds as a no-op.
        clear_stale_ready(&path).unwrap();
        assert_eq!(state::read(&path).unwrap().unwrap().ready, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- C3: restart-policy exit decision ---

    #[test]
    fn is_failure_only_clean_exit_is_success() {
        let pid = Pid::from_raw(1);
        assert!(!is_failure(WaitStatus::Exited(pid, 0)));
        assert!(is_failure(WaitStatus::Exited(pid, 1)));
        assert!(is_failure(WaitStatus::Signaled(
            pid,
            Signal::SIGKILL,
            false
        )));
    }

    #[test]
    fn exit_action_off_always_mirrors_child() {
        // restart=off: a clean exit and a crash both exit lode with the code.
        assert_eq!(
            exit_action(RestartPolicy::Off, None, false, 0, 0, 0, 1, 30),
            ExitAction::Exit { code: 0 }
        );
        assert_eq!(
            exit_action(RestartPolicy::Off, None, true, 7, 0, 0, 1, 30),
            ExitAction::Exit { code: 7 }
        );
    }

    #[test]
    fn exit_action_on_failure_exits_clean_restarts_crash() {
        // Clean exit → exit 0 (mirror the intentional shutdown).
        assert_eq!(
            exit_action(RestartPolicy::OnFailure, None, false, 0, 0, 0, 1, 30),
            ExitAction::Exit { code: 0 }
        );
        // Failure → restart with the base backoff.
        assert_eq!(
            exit_action(RestartPolicy::OnFailure, None, true, 137, 0, 0, 1, 30),
            ExitAction::Restart(Duration::from_secs(1))
        );
    }

    #[test]
    fn exit_action_always_restarts_then_pauses_at_cap() {
        // Clean exit still restarts; backoff doubles with the consecutive count.
        assert_eq!(
            exit_action(RestartPolicy::Always, None, false, 0, 0, 0, 1, 30),
            ExitAction::Restart(Duration::from_secs(1))
        );
        assert_eq!(
            exit_action(RestartPolicy::Always, None, false, 0, 2, 0, 1, 30),
            ExitAction::Restart(Duration::from_secs(4))
        );
        // restart_max=2 allows 2 retries; the 3rd failure (restarts already 2) PAUSES
        // (keep-alive) rather than exiting — lode stays alive as PID 1.
        assert_eq!(
            exit_action(RestartPolicy::Always, None, true, 3, 2, 2, 1, 30),
            ExitAction::Pause
        );
        // on-failure pauses at the cap the same way (restarts==restart_max==3).
        assert_eq!(
            exit_action(RestartPolicy::OnFailure, None, true, 1, 3, 3, 1, 30),
            ExitAction::Pause
        );
    }

    #[test]
    fn exit_action_pending_update_wins_over_policy() {
        // A pending different target applies the update regardless of policy —
        // even restart=off (mirror) and even past the retry cap.
        assert_eq!(
            exit_action(RestartPolicy::Off, Some("2.0.0"), false, 0, 0, 0, 1, 30),
            ExitAction::ApplyUpdate("2.0.0".to_owned())
        );
        assert_eq!(
            exit_action(RestartPolicy::Always, Some("2.0.0"), true, 1, 9, 2, 1, 30),
            ExitAction::ApplyUpdate("2.0.0".to_owned())
        );
    }

    // --- C2/C3: target-application observation state transitions ---

    #[test]
    fn observe_decision_commit_rollback_pending() {
        // Ready wins, even if it also timed out.
        assert_eq!(observe_decision(true, true), ObserveOutcome::Commit);
        assert_eq!(observe_decision(true, false), ObserveOutcome::Commit);
        // Not ready + timed out → rollback (single-strike).
        assert_eq!(observe_decision(false, true), ObserveOutcome::Rollback);
        // Not ready, no timeout → keep waiting.
        assert_eq!(observe_decision(false, false), ObserveOutcome::Pending);
    }

    // --- C2: history append + cap ---

    #[test]
    fn push_history_appends_and_caps() {
        let mut history = Vec::new();
        push_history(&mut history, "1.0.0", HistoryResult::Good, "t0".to_owned());
        push_history(&mut history, "1.1.0", HistoryResult::Bad, "t1".to_owned());
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].version, "1.0.0");
        assert_eq!(history[1].result, HistoryResult::Bad);

        // Exceeding the cap drops the oldest entries first.
        for i in 0..HISTORY_CAP {
            push_history(
                &mut history,
                &format!("9.0.{i}"),
                HistoryResult::Good,
                "t".to_owned(),
            );
        }
        assert_eq!(history.len(), HISTORY_CAP);
        // The two seed entries fell off the front.
        assert_eq!(history[0].version, "9.0.0");
    }

    // --- C2: timestamp formatting ---

    #[test]
    fn format_rfc3339_known_epochs() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        // A leap-day instant: 2024-02-29T12:00:00Z.
        assert_eq!(format_rfc3339(1_709_208_000), "2024-02-29T12:00:00Z");
    }

    #[test]
    fn readiness_label_maps_modes() {
        assert_eq!(readiness_label(Readiness::None), "none");
        assert_eq!(readiness_label(Readiness::State), "state");
    }

    // --- PID-1 hardening: best-effort state writes + bootstrap signal handling ---

    /// A minimal config rooted at `data_dir` for supervisor-level tests.
    fn test_config(data_dir: PathBuf) -> Config {
        Config {
            global: config::Global {
                app: "myapp".to_owned(),
                data_dir,
                log_level: "info".to_owned(),
            },
            update: config::Update {
                manifest: None,
                github: None,
                github_api: "https://api.github.com".to_owned(),
                asset: None,
                channel: "stable".to_owned(),
                policy: Policy::Off,
                check_interval: 0,
                keep_versions: 3,
                pin: None,
            },
            http: config::Http {
                headers: Vec::new(),
                credential_hosts: Vec::new(),
                allow_insecure: false,
            },
            trust: config::Trust {
                require_signature: config::RequireSignature::Off,
                trusted_keys: Vec::new(),
                trusted_keys_file: None,
            },
            command: config::Command {
                run: Some("./app".to_owned()),
                exec: Some("./app".to_owned()),
                workdir: "{dir}".to_owned(),
            },
            runtime: config::Runtime {
                runtime: None,
                download: None,
                version: None,
                version_check: None,
            },
            supervise: config::Supervise {
                restart: RestartPolicy::OnFailure,
                restart_backoff: 1,
                restart_backoff_max: 30,
                restart_max: 3,
                readiness: Readiness::None,
                ready_timeout: 30,
                prepare_timeout: 0,
                health_grace: 15,
                stop_timeout: 10,
                restart_mode: crate::config::RestartMode::StopStart,
                listen: None,
            },
            signals: config::Signals {
                forward: Vec::new(),
                restart: None,
            },
            env: BTreeMap::new(),
            config_path: None,
        }
    }

    fn test_target(dir: &Path) -> Target {
        Target {
            version: "1.0.0".to_owned(),
            dir: dir.to_path_buf(),
            run: None,
            exec: None,
        }
    }

    #[test]
    fn mutate_state_and_enter_paused_survive_an_unwritable_state_path() {
        let dir = std::env::temp_dir().join(format!("lode-badstate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // data_dir is a regular FILE, so every state.json read/write under it fails
        // (ENOTDIR) — unlike a chmod-only read-only dir, this fails for root too.
        let data_dir = dir.join("notadir");
        std::fs::write(&data_dir, b"not a directory").unwrap();
        let cfg = test_config(data_dir);
        let mut sup = Supervisor::new(&cfg, test_target(&dir), None);

        // Best-effort: a failing read+write logs and returns — never an Err/panic.
        sup.mutate_state(|st| st.status = Some(Status::Error));
        sup.set_status(Status::Running);
        sup.note_error("disk is gone");

        // The keep-alive pause must take effect in memory even when the state
        // write fails — a full/readonly disk must not defeat it (design §8).
        sup.enter_paused(7);
        assert!(
            sup.paused,
            "pause must engage despite a failing state write"
        );
        assert!(sup.child.is_none());
        assert!(sup.restart_at.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mtime_probes_survive_an_unstatable_path() {
        // R2-2: a non-NotFound stat error (here ENOTDIR via a regular file as a
        // path component — fails for root too) must skip the tick / report
        // "unchanged", never propagate an Err that would exit PID 1.
        let dir = std::env::temp_dir().join(format!("lode-badmtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let data_dir = dir.join("notadir");
        std::fs::write(&data_dir, b"not a directory").unwrap();
        let mut cfg = test_config(data_dir.clone());
        cfg.config_path = Some(data_dir.join("lode.toml"));
        let mut sup = Supervisor::new(&cfg, test_target(&dir), None);

        // The probe itself errors (precondition for the regression).
        assert!(state::mtime(&data_dir.join("state.json")).is_err());

        assert!(sup.poll_state().is_none(), "poll_state must skip the tick");
        assert!(
            !sup.config_changed(),
            "config_changed must report unchanged"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bootstrap_terminated_acts_on_a_pending_sigterm() {
        let dir = std::env::temp_dir().join(format!("lode-bootsig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = test_config(dir.clone());
        let mut signals = Signals::new(registration_set(&default_forward(), None)).unwrap();

        // No signal pending => carry on with bootstrap.
        assert!(bootstrap_terminated(&mut signals, &cfg).is_none());

        // A SIGTERM raised at ourselves (nextest: one process per test) must be
        // picked up between bootstrap steps; delivery is async, so poll briefly.
        kill(Pid::this(), Signal::SIGTERM).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut code = None;
        while code.is_none() && Instant::now() < deadline {
            code = bootstrap_terminated(&mut signals, &cfg);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(code.is_some(), "pending SIGTERM must terminate bootstrap");

        // The terminal status was written (best-effort path had a working disk).
        let st = state::read(&dir.join("state.json")).unwrap().unwrap();
        assert_eq!(st.status, Some(Status::Stopped));
        assert_eq!(st.pid, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registration_set_merges_and_dedups() {
        // Termination set + forward set + restart signal, deduplicated.
        let set = registration_set(&[Signal::SIGHUP, Signal::SIGTERM], Some(Signal::SIGHUP));
        assert_eq!(
            set,
            vec![
                Signal::SIGTERM as c_int,
                Signal::SIGINT as c_int,
                Signal::SIGQUIT as c_int,
                Signal::SIGHUP as c_int,
            ]
        );
        // Forbidden signals are dropped rather than registered.
        let set = registration_set(&[Signal::SIGKILL], None);
        assert!(!set.contains(&(Signal::SIGKILL as c_int)));
    }
}
