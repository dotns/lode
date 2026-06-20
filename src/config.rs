//! Configuration: the resolved [`Config`] plus its loader.
//!
//! Precedence is `CLI > env (LODE_*) > lode.toml > default` (design §10). clap
//! folds env into each CLI field (every global arg carries `env = "LODE_…"`), so
//! [`merge`] sees just two layers — the CLI/env slot and the parsed TOML — and
//! the design's default table fills the rest. CLI-over-env within the first slot
//! is clap's contract; this module owns env/toml-over-default.
//!
//! Header values and trusted keys are stored verbatim and never expanded or
//! logged here; `${ENV}` expansion happens at fetch time in the http module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::cli::Globals;
use crate::error::{Error, Result};

const DEFAULT_LOG_LEVEL: &str = "info";
const DEFAULT_APP: &str = "app";
/// Default base / run directory. Holds `lode.toml`, `versions/`, `state.json`,
/// `lode.pid` and `runtime/`. Change the whole location with `--data-dir` /
/// `LODE_DATA_DIR` (config is then searched at `$DATA_DIR/lode.toml`).
const DEFAULT_DATA_DIR: &str = "/srv/lode";

/// Minimal starter `lode.toml` scaffolded on first run when none exists (also
/// what `lode-cli init` writes). Deliberately small — the complete documented
/// reference lives in `docs/lode.example.toml` (which a test also parses).
pub(crate) const STARTER_TOML: &str = include_str!("../docs/lode.starter.toml");
const DEFAULT_GITHUB_API: &str = "https://api.github.com";
const DEFAULT_CHANNEL: &str = "stable";
const DEFAULT_WORKDIR_PLACEHOLDER: &str = "{dir}";

// --- typed enums (shared by the CLI and TOML layers) -----------------------

/// Update policy (`update.policy`), design §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Policy {
    /// No background checks; run current/pinned only.
    Off,
    /// Periodically check and advertise, but never auto-apply (default).
    Check,
    /// Periodically check and auto-apply newer versions.
    Auto,
}

/// Signature-enforcement strength (`trust.require_signature`), design §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RequireSignature {
    /// Integrity only (sha256), no signature check.
    Off,
    /// Enforce when keys are configured, else warn-and-skip (default).
    Auto,
    /// Always require a valid signature.
    Enforce,
}

/// Readiness determination (`supervise.readiness`), design §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Readiness {
    /// Alive for `health_grace` seconds counts as ready (default).
    None,
    /// Wait for the app to write `state.ready`.
    State,
}

/// Crash-restart policy (`supervise.restart`), design §8. Gates the bounded
/// backoff machinery and the keep-alive *pause*; on `off` lode mirrors the child's
/// lifecycle. Update / rollback / explicit-restart relaunches happen regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RestartPolicy {
    /// Mirror the child: lode exits with the child's code, never restarting. The
    /// orchestrator owns whole-process restart.
    Off,
    /// Restart on failure (non-zero exit or killed by a signal); a clean `exit(0)`
    /// makes lode exit too. After `restart_max` failed retries lode PAUSES (stays
    /// alive — does not exit) until a recovery trigger. The default.
    OnFailure,
    /// Like `on-failure` but restart on *any* child exit, including a clean
    /// `exit(0)`; pauses after `restart_max` retries.
    Always,
}

/// Restart strategy (`supervise.restart_mode`), design §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RestartMode {
    /// Stop the old child before starting the new one (default).
    StopStart,
    /// systemd socket-activation protocol; needs `listen`.
    SocketActivation,
    /// Overlap old and new via `SO_REUSEPORT`.
    ReuseportOverlap,
}

// --- resolved config (the 8 sections of design §10) ------------------------

/// Fully resolved lode configuration.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) global: Global,
    pub(crate) update: Update,
    pub(crate) http: Http,
    pub(crate) trust: Trust,
    pub(crate) command: Command,
    pub(crate) runtime: Runtime,
    pub(crate) supervise: Supervise,
    pub(crate) signals: Signals,
    /// `[env]` — extra environment variables injected into the child (on top of
    /// the inherited host env; lode's own `LODE_*` introspection vars still win).
    pub(crate) env: BTreeMap<String, String>,
    /// The `lode.toml` path this config was read from (`None` when running
    /// file-less). The supervisor watches its mtime to recover a *paused* app after
    /// the operator edits it (design §8); a running app is never disturbed.
    #[allow(clippy::struct_field_names)] // mirrors the on-disk `lode.toml` concept
    pub(crate) config_path: Option<PathBuf>,
}

/// `[global]` — identity and storage.
#[derive(Debug, Clone)]
pub(crate) struct Global {
    pub(crate) app: String,
    pub(crate) data_dir: PathBuf,
    pub(crate) log_level: String,
}

/// `[update]` — source and upgrade policy.
#[derive(Debug, Clone)]
pub(crate) struct Update {
    /// native source (mutually exclusive with [`Self::github`]).
    pub(crate) manifest: Option<String>,
    /// github source (mutually exclusive with [`Self::manifest`]).
    pub(crate) github: Option<String>,
    pub(crate) github_api: String,
    /// The asset filename to install on this host — the source-agnostic selection
    /// key (source-adapters §3/§7). Required to resolve a download.
    pub(crate) asset: Option<String>,
    pub(crate) channel: String,
    pub(crate) policy: Policy,
    pub(crate) check_interval: u64,
    pub(crate) keep_versions: u32,
    pub(crate) pin: Option<String>,
}

/// `[http]` — fetch credentials. Values are stored raw (never expanded/logged).
#[derive(Debug, Clone)]
pub(crate) struct Http {
    pub(crate) headers: Vec<String>,
    /// Extra hosts (beyond the manifest/source origin) that may receive
    /// [`Self::headers`] on an artifact download. Empty by default; same-origin
    /// is always allowed (see [`crate::download::fetch_artifact`]).
    pub(crate) credential_hosts: Vec<String>,
    /// Permit non-HTTPS (plain `http`) remote fetches. Off by default; loopback
    /// http is always allowed regardless. See [`crate::http`].
    pub(crate) allow_insecure: bool,
}

/// `[trust]` — publisher-identity verification.
#[derive(Debug, Clone)]
pub(crate) struct Trust {
    pub(crate) require_signature: RequireSignature,
    pub(crate) trusted_keys: Vec<String>,
    pub(crate) trusted_keys_file: Option<String>,
}

/// `[command]` — how to launch the app. `run`/`exec` are LITERAL commands
/// (whitespace-split; only `{dir}` expands, to the running version dir) and are
/// optional here: a manifest asset may publish signed `run`/`exec` overrides that
/// take precedence, and launch resolution errors only when *neither* side supplies
/// the command actually needed (see `supervisor::effective_command`).
#[derive(Debug, Clone)]
pub(crate) struct Command {
    pub(crate) run: Option<String>,
    pub(crate) exec: Option<String>,
    pub(crate) workdir: String,
}

/// `[runtime]` — optional runtime dependency.
// `runtime` mirrors the `[runtime] runtime = "…"` TOML key, so the field name is
// fixed by the schema even though it repeats the struct name.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone)]
pub(crate) struct Runtime {
    pub(crate) runtime: Option<String>,
    pub(crate) download: Option<String>,
    /// Expected runtime version. When set, lode probes the runtime (PATH, cache,
    /// or freshly downloaded) and requires its self-reported version to *contain*
    /// this string; a wrong-version PATH/cache entry is bypassed for a fresh
    /// download, and a downloaded mismatch is a hard error.
    pub(crate) version: Option<String>,
    /// Argument(s) that make the runtime print its version (whitespace-split,
    /// appended to the runtime binary). Defaults to `--version`. Only used when
    /// [`version`](Self::version) is set.
    pub(crate) version_check: Option<String>,
}

/// `[supervise]` — restart policy / health / rollback / stop / restart mode.
#[derive(Debug, Clone)]
pub(crate) struct Supervise {
    /// Crash-restart policy. `on-failure` (default) retries then pauses (keep-alive);
    /// `off` mirrors the child; `always` also retries a clean exit.
    pub(crate) restart: RestartPolicy,
    /// Backoff base/cap **in seconds** and the retry cap before pausing — only used
    /// when [`restart`](Self::restart) is not `off`. `restart_max=0` retries forever.
    pub(crate) restart_backoff: u64,
    pub(crate) restart_backoff_max: u64,
    pub(crate) restart_max: u32,
    pub(crate) readiness: Readiness,
    pub(crate) ready_timeout: u64,
    /// (`readiness=state`) Seconds to wait for the app to ack a staged-update
    /// prepare prompt before forcing the cut-over; `0` (default) disables the
    /// timeout — the app paces the cut-over (design §8).
    pub(crate) prepare_timeout: u64,
    pub(crate) health_grace: u64,
    pub(crate) stop_timeout: u64,
    pub(crate) restart_mode: RestartMode,
    pub(crate) listen: Option<String>,
}

/// `[signals]` — signal forwarding. Empty `forward` => lode's standard set (§8).
#[derive(Debug, Clone)]
pub(crate) struct Signals {
    pub(crate) forward: Vec<String>,
    pub(crate) restart: Option<String>,
}

// --- raw TOML layer (everything optional) ----------------------------------
//
// Every struct is `deny_unknown_fields`: a typo'd key in `lode.toml` is a hard
// parse error naming the key, not a silent no-op. This matters most for the
// keep-alive pause-recovery flow, where the operator fixes the file while the
// app is paused — a silently ignored key would leave it paused with no hint.
// (`[env]` stays open: it is a BTreeMap of user-defined variables.)

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlConfig {
    #[serde(default)]
    global: TomlGlobal,
    #[serde(default)]
    update: TomlUpdate,
    #[serde(default)]
    http: TomlHttp,
    #[serde(default)]
    trust: TomlTrust,
    #[serde(default)]
    command: TomlCommand,
    #[serde(default)]
    runtime: TomlRuntime,
    #[serde(default)]
    supervise: TomlSupervise,
    #[serde(default)]
    signals: TomlSignals,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlGlobal {
    app: Option<String>,
    data_dir: Option<String>,
    log_level: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlUpdate {
    manifest: Option<String>,
    github: Option<String>,
    github_api: Option<String>,
    asset: Option<String>,
    channel: Option<String>,
    policy: Option<Policy>,
    check_interval: Option<u64>,
    keep_versions: Option<u32>,
    pin: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlHttp {
    headers: Option<Vec<String>>,
    credential_hosts: Option<Vec<String>>,
    allow_insecure: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlTrust {
    require_signature: Option<RequireSignature>,
    trusted_keys: Option<Vec<String>>,
    trusted_keys_file: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlCommand {
    run: Option<String>,
    exec: Option<String>,
    workdir: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlRuntime {
    runtime: Option<String>,
    download: Option<String>,
    version: Option<String>,
    version_check: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlSupervise {
    restart: Option<RestartPolicy>,
    restart_backoff: Option<u64>,
    restart_backoff_max: Option<u64>,
    restart_max: Option<u32>,
    readiness: Option<Readiness>,
    ready_timeout: Option<u64>,
    prepare_timeout: Option<u64>,
    health_grace: Option<u64>,
    stop_timeout: Option<u64>,
    restart_mode: Option<RestartMode>,
    listen: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlSignals {
    forward: Option<Vec<String>>,
    restart: Option<String>,
}

// --- resolution ------------------------------------------------------------

/// Resolve the effective configuration from CLI/env (`cli`), `lode.toml` and the
/// design defaults, then validate it.
pub(crate) fn resolve(cli: &Globals) -> Result<Config> {
    let (toml, config_path) = load_toml(cli)?;
    let mut cfg = merge(cli, &toml);
    cfg.config_path = config_path;
    validate(&cfg)?;
    Ok(cfg)
}

/// Write a minimal **sourceless** `lode.toml` (no update source, `policy=off`) at
/// `$DATA_DIR/lode.toml` when one is absent, so a `seed`-prepared data dir runs
/// offline with bare `lode`. Never clobbers an existing config. Used by
/// `lode-cli seed` before it resolves, so seeding "just works" on a fresh dir
/// without tripping the source-requiring starter scaffold. `seed_source` is the
/// file being seeded — its name derives the scaffolded `[command]` launch command.
pub(crate) fn ensure_sourceless_toml(cli: &Globals, seed_source: &Path) -> Result<()> {
    let data_dir = cli.data_dir.as_deref().unwrap_or(DEFAULT_DATA_DIR);
    let path = Path::new(data_dir).join("lode.toml");
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Config(format!("create {}: {e}", dir.display())))?;
    }
    let app = cli.app.as_deref().unwrap_or(DEFAULT_APP);
    let command = seed_run_command(seed_source, app);
    let body = format!(
        "# Sourceless config for OFFLINE local testing (written by `lode-cli seed`).\n\
         # No [update].manifest/github => lode never downloads; it runs whatever is\n\
         # seeded under versions/. policy=off disables the background update check.\n\
         [global]\n\
         app = \"{app}\"\n\n\
         [update]\n\
         policy = \"off\"\n\n\
         # Literal launch commands (cwd = the version dir); edit if the seeded\n\
         # artifact needs a runtime (e.g. run = \"bun run app.ts\").\n\
         [command]\n\
         run  = \"{command}\"\n\
         exec = \"{command}\"\n"
    );
    std::fs::write(&path, body)
        .map_err(|e| Error::Config(format!("write {}: {e}", path.display())))?;
    tracing::info!(path = %path.display(), "seed: wrote sourceless lode.toml");
    Ok(())
}

/// Best-effort launch command for a seeded artifact, mirroring where
/// `install::seed_local` lands the file: a raw file keeps its filename, a `.gz`
/// drops the suffix, and an archive's binary is conventionally `./{app}` at the
/// extraction root (the scaffold comment tells the operator to edit otherwise).
fn seed_run_command(seed_source: &Path, app: &str) -> String {
    let name = seed_source
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(app);
    match crate::manifest::format_from_name(name) {
        "raw" => format!("./{name}"),
        "gz" => format!("./{}", name.strip_suffix(".gz").unwrap_or(name)),
        _ => format!("./{app}"),
    }
}

/// Locate `lode.toml` without reading it: an explicit `--config`/`LODE_CONFIG`
/// always wins (even if the file is missing — the read will report it);
/// otherwise search `$DATA_DIR/lode.toml`, then `./lode.toml`. Shared by
/// [`load_toml`] and [`peek_log_level`] so both see the same file.
fn find_config_path(cli: &Globals) -> Option<PathBuf> {
    if let Some(path) = cli.config.as_ref() {
        return Some(PathBuf::from(path));
    }
    let data_dir = cli.data_dir.as_deref().unwrap_or(DEFAULT_DATA_DIR);
    let in_data = Path::new(data_dir).join("lode.toml");
    if in_data.is_file() {
        return Some(in_data);
    }
    let local = PathBuf::from("lode.toml");
    if local.is_file() { Some(local) } else { None }
}

/// Cheap pre-logging peek at `[global].log_level`, so `logging::init` (which
/// must run before [`resolve`]) can honour the TOML value. Lenient by design:
/// a missing, unreadable or malformed file yields `None` — the subsequent full
/// resolve reports the real error with logging already up.
pub(crate) fn peek_log_level(cli: &Globals) -> Option<String> {
    let path = find_config_path(cli)?;
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: TomlConfig = toml::from_str(&text).ok()?;
    parsed.global.log_level
}

/// Locate and parse `lode.toml`. An explicit `--config`/`LODE_CONFIG` must exist;
/// otherwise the default search (`$DATA_DIR/lode.toml`, then `./lode.toml`) is
/// best-effort and a missing file yields the all-defaults config (design §15).
fn load_toml(cli: &Globals) -> Result<(TomlConfig, Option<PathBuf>)> {
    let Some(path) = find_config_path(cli) else {
        // No lode.toml anywhere. A source given via env/CLI lets us run file-less;
        // otherwise scaffold a starter at `$DATA_DIR/lode.toml` and guide the
        // operator to fill it in (design §15).
        if cli.manifest.is_some() || cli.github.is_some() {
            return Ok((TomlConfig::default(), None));
        }
        let data_dir = cli.data_dir.as_deref().unwrap_or(DEFAULT_DATA_DIR);
        return Err(scaffold_starter_config(
            &Path::new(data_dir).join("lode.toml"),
        ));
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::Config(format!("read config {}: {e}", path.display())))?;
    Ok((toml::from_str(&text)?, Some(path)))
}

/// First-run convenience: write a starter `lode.toml` at `path` (best-effort,
/// creating parent dirs) and return a guiding error so the loader stops cleanly
/// instead of failing later on a placeholder source.
fn scaffold_starter_config(path: &Path) -> Error {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(path, STARTER_TOML) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "no lode.toml found — wrote a starter config");
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not write starter lode.toml");
            }
        }
    }
    Error::Config(format!(
        "no lode.toml found — wrote a starter to {}; set [update].manifest (or [update].github) \
         to your real source and re-run, or pass --manifest/--github (LODE_MANIFEST/LODE_GITHUB)",
        path.display()
    ))
}

/// Merge the CLI/env layer over the TOML layer over the defaults. Infallible;
/// semantic checks live in [`validate`].
fn merge(cli: &Globals, t: &TomlConfig) -> Config {
    Config {
        global: merge_global(cli, &t.global),
        update: merge_update(cli, &t.update),
        http: merge_http(cli, &t.http),
        trust: merge_trust(cli, &t.trust),
        command: merge_command(cli, &t.command),
        runtime: merge_runtime(cli, &t.runtime),
        supervise: merge_supervise(cli, &t.supervise),
        signals: merge_signals(cli, &t.signals),
        // `[env]` is config-file only — no CLI/env override layer. To override an
        // entry at deploy time, set it directly in the process env (it wins as a
        // host env var; see `child_env`).
        env: t.env.clone(),
        config_path: None, // filled in by `resolve` after merge
    }
}

fn merge_global(cli: &Globals, t: &TomlGlobal) -> Global {
    Global {
        app: cli
            .app
            .clone()
            .or_else(|| t.app.clone())
            .unwrap_or_else(|| DEFAULT_APP.to_owned()),
        data_dir: cli
            .data_dir
            .clone()
            .or_else(|| t.data_dir.clone())
            .map_or_else(|| PathBuf::from(DEFAULT_DATA_DIR), PathBuf::from),
        log_level: cli
            .log_level
            .clone()
            .or_else(|| t.log_level.clone())
            .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_owned()),
    }
}

fn merge_update(cli: &Globals, t: &TomlUpdate) -> Update {
    Update {
        manifest: cli.manifest.clone().or_else(|| t.manifest.clone()),
        github: cli.github.clone().or_else(|| t.github.clone()),
        github_api: cli
            .github_api
            .clone()
            .or_else(|| t.github_api.clone())
            .unwrap_or_else(|| DEFAULT_GITHUB_API.to_owned()),
        asset: cli.asset.clone().or_else(|| t.asset.clone()),
        channel: cli
            .channel
            .clone()
            .or_else(|| t.channel.clone())
            .unwrap_or_else(|| DEFAULT_CHANNEL.to_owned()),
        policy: cli.policy.or(t.policy).unwrap_or(Policy::Check),
        check_interval: cli.interval.or(t.check_interval).unwrap_or(300),
        keep_versions: cli.keep.or(t.keep_versions).unwrap_or(3),
        pin: cli.pin.clone().or_else(|| t.pin.clone()),
    }
}

fn merge_http(cli: &Globals, t: &TomlHttp) -> Http {
    let headers = if cli.header.is_empty() {
        t.headers.clone().unwrap_or_default()
    } else {
        cli.header.clone()
    };
    let credential_hosts = if cli.credential_host.is_empty() {
        t.credential_hosts.clone().unwrap_or_default()
    } else {
        cli.credential_host.clone()
    };
    // `--allow-insecure-http` is a one-way switch (it can only turn the gate on),
    // so CLI-true wins; otherwise fall back to the TOML value, default false.
    let allow_insecure = cli.allow_insecure_http || t.allow_insecure.unwrap_or(false);
    Http {
        headers,
        credential_hosts,
        allow_insecure,
    }
}

fn merge_trust(cli: &Globals, t: &TomlTrust) -> Trust {
    let trusted_keys = cli.trusted_keys.as_ref().map_or_else(
        || t.trusted_keys.clone().unwrap_or_default(),
        |list| split_csv(list),
    );
    Trust {
        require_signature: cli
            .require_signature
            .or(t.require_signature)
            .unwrap_or(RequireSignature::Auto),
        trusted_keys,
        trusted_keys_file: cli
            .trusted_keys_file
            .clone()
            .or_else(|| t.trusted_keys_file.clone()),
    }
}

fn merge_command(cli: &Globals, t: &TomlCommand) -> Command {
    // `run`/`exec` have NO default: an unset command stays `None`, and launch
    // resolution decides (a manifest asset may supply the override; otherwise it
    // is a clear "no run command" error at launch, not at config parse).
    Command {
        run: cli.run.clone().or_else(|| t.run.clone()),
        exec: cli.exec.clone().or_else(|| t.exec.clone()),
        workdir: cli
            .workdir
            .clone()
            .or_else(|| t.workdir.clone())
            .unwrap_or_else(|| DEFAULT_WORKDIR_PLACEHOLDER.to_owned()),
    }
}

fn merge_runtime(cli: &Globals, t: &TomlRuntime) -> Runtime {
    Runtime {
        runtime: cli.runtime.clone().or_else(|| t.runtime.clone()),
        download: cli.runtime_download.clone().or_else(|| t.download.clone()),
        version: cli.runtime_version.clone().or_else(|| t.version.clone()),
        version_check: cli
            .runtime_version_check
            .clone()
            .or_else(|| t.version_check.clone()),
    }
}

fn merge_supervise(cli: &Globals, t: &TomlSupervise) -> Supervise {
    Supervise {
        // Default keep-alive (design §8): a failing app is retried, then the
        // supervisor PAUSES (stays alive — never crash-loops the container) rather
        // than exiting. `off` opts back into mirror-the-child (lode exits with it).
        restart: cli
            .restart
            .or(t.restart)
            .unwrap_or(RestartPolicy::OnFailure),
        restart_backoff: cli.restart_backoff.or(t.restart_backoff).unwrap_or(1),
        restart_backoff_max: cli
            .restart_backoff_max
            .or(t.restart_backoff_max)
            .unwrap_or(30),
        // Retry this many times after a failure, then pause (0 = retry forever).
        restart_max: cli.restart_max.or(t.restart_max).unwrap_or(3),
        readiness: cli.readiness.or(t.readiness).unwrap_or(Readiness::None),
        ready_timeout: cli.ready_timeout.or(t.ready_timeout).unwrap_or(30),
        // TOML-only (no CLI flag): an advanced staged-update knob. 0 = disabled,
        // the app paces the cut-over (the documented default behaviour).
        prepare_timeout: t.prepare_timeout.unwrap_or(0),
        health_grace: cli.health_grace.or(t.health_grace).unwrap_or(15),
        stop_timeout: cli.stop_timeout.or(t.stop_timeout).unwrap_or(10),
        restart_mode: cli
            .restart_mode
            .or(t.restart_mode)
            .unwrap_or(RestartMode::StopStart),
        listen: cli.listen.clone().or_else(|| t.listen.clone()),
    }
}

fn merge_signals(cli: &Globals, t: &TomlSignals) -> Signals {
    let forward = cli.forward_signals.as_ref().map_or_else(
        || t.forward.clone().unwrap_or_default(),
        |list| split_csv(list),
    );
    Signals {
        forward,
        restart: cli.restart_signal.clone().or_else(|| t.restart.clone()),
    }
}

/// Split a comma-separated list, trimming whitespace and dropping empties.
fn split_csv(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Semantic validation: non-empty key fields, source XOR, and numeric ranges.
/// (Enum values are already checked by clap/serde at parse time.)
fn validate(cfg: &Config) -> Result<()> {
    if cfg.global.app.trim().is_empty() {
        return Err(Error::Config(
            "global.app must not be empty (the app name namespaces the data dir and lock)"
                .to_owned(),
        ));
    }
    if cfg.update.channel.trim().is_empty() {
        return Err(Error::Config(
            "update.channel must not be empty (e.g. \"stable\")".to_owned(),
        ));
    }
    if let Some(asset) = &cfg.update.asset
        && asset.trim().is_empty()
    {
        return Err(Error::Config(
            "update.asset must not be empty (set the asset filename to install, or omit it)"
                .to_owned(),
        ));
    }
    match (&cfg.update.manifest, &cfg.update.github) {
        (Some(_), Some(_)) => {
            return Err(Error::Config(
                "update.manifest and update.github are mutually exclusive (set exactly one)"
                    .to_owned(),
            ));
        }
        (None, None) => {
            tracing::debug!("no update source configured (neither manifest nor github set)");
        }
        _ => {}
    }
    if cfg.supervise.restart_backoff_max < cfg.supervise.restart_backoff {
        return Err(Error::Config(format!(
            "supervise.restart_backoff_max ({}) must be >= restart_backoff ({})",
            cfg.supervise.restart_backoff_max, cfg.supervise.restart_backoff
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Globals` with no flags set (the all-`None` baseline), so [`merge`] sees
    /// an empty CLI/env layer.
    fn blank_cli() -> Globals {
        Globals {
            log_level: None,
            config: None,
            app: None,
            data_dir: None,
            manifest: None,
            github: None,
            github_api: None,
            asset: None,
            channel: None,
            policy: None,
            interval: None,
            keep: None,
            pin: None,
            header: Vec::new(),
            credential_host: Vec::new(),
            allow_insecure_http: false,
            require_signature: None,
            trusted_keys: None,
            trusted_keys_file: None,
            run: None,
            exec: None,
            workdir: None,
            runtime: None,
            runtime_download: None,
            runtime_version: None,
            runtime_version_check: None,
            restart: None,
            restart_backoff: None,
            restart_backoff_max: None,
            restart_max: None,
            readiness: None,
            ready_timeout: None,
            health_grace: None,
            stop_timeout: None,
            restart_mode: None,
            listen: None,
            forward_signals: None,
            restart_signal: None,
        }
    }

    #[test]
    fn default_fallback() {
        let cfg = merge(&blank_cli(), &TomlConfig::default());
        assert_eq!(cfg.global.app, "app");
        assert_eq!(cfg.global.data_dir, PathBuf::from("/srv/lode"));
        assert_eq!(cfg.global.log_level, "info");
        assert_eq!(cfg.update.policy, Policy::Check);
        assert_eq!(cfg.update.check_interval, 300);
        assert_eq!(cfg.update.keep_versions, 3);
        assert_eq!(cfg.update.channel, "stable");
        assert_eq!(cfg.update.github_api, "https://api.github.com");
        assert_eq!(cfg.trust.require_signature, RequireSignature::Auto);
        // No default launch command: unset stays None (resolved at launch, where a
        // manifest asset override may supply it).
        assert_eq!(cfg.command.run, None);
        assert_eq!(cfg.command.exec, None);
        assert_eq!(cfg.command.workdir, "{dir}");
        assert_eq!(cfg.supervise.readiness, Readiness::None);
        // Keep-alive by default: retry a failing app, then pause (don't exit).
        assert_eq!(cfg.supervise.restart, RestartPolicy::OnFailure);
        assert_eq!(cfg.supervise.restart_max, 3);
        assert_eq!(cfg.supervise.restart_mode, RestartMode::StopStart);
        assert_eq!(cfg.supervise.restart_backoff, 1);
        // prepare_timeout defaults to 0 = disabled (app-paced cut-over, §8).
        assert_eq!(cfg.supervise.prepare_timeout, 0);
        assert!(cfg.http.headers.is_empty());
        assert!(cfg.http.credential_hosts.is_empty());
        assert!(!cfg.http.allow_insecure); // secure by default
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn credential_hosts_from_toml_then_cli_overrides() {
        // TOML supplies the allowlist when the CLI slot is empty…
        let t = TomlHttp {
            credential_hosts: Some(vec!["cdn.example".to_owned()]),
            ..TomlHttp::default()
        };
        assert_eq!(
            merge_http(&blank_cli(), &t).credential_hosts,
            vec!["cdn.example".to_owned()]
        );
        // …and `--credential-host` (repeatable) overrides it entirely.
        let mut cli = blank_cli();
        cli.credential_host = vec!["a.example".to_owned(), "b.example".to_owned()];
        assert_eq!(
            merge_http(&cli, &t).credential_hosts,
            vec!["a.example".to_owned(), "b.example".to_owned()]
        );
    }

    #[test]
    fn allow_insecure_http_precedence() {
        // Default: the gate is off.
        assert!(
            !merge(&blank_cli(), &TomlConfig::default())
                .http
                .allow_insecure
        );

        // TOML opts in.
        let t = TomlConfig {
            http: TomlHttp {
                allow_insecure: Some(true),
                ..TomlHttp::default()
            },
            ..TomlConfig::default()
        };
        assert!(merge(&blank_cli(), &t).http.allow_insecure);

        // The CLI flag forces it on even when TOML is unset/false.
        let mut cli = blank_cli();
        cli.allow_insecure_http = true;
        assert!(merge(&cli, &TomlConfig::default()).http.allow_insecure);
    }

    #[test]
    fn toml_only() {
        let t = TomlConfig {
            global: TomlGlobal {
                app: Some("myapp".to_owned()),
                ..TomlGlobal::default()
            },
            update: TomlUpdate {
                policy: Some(Policy::Auto),
                check_interval: Some(60),
                manifest: Some("https://example.com/m.json".to_owned()),
                ..TomlUpdate::default()
            },
            trust: TomlTrust {
                trusted_keys: Some(vec!["id:key".to_owned()]),
                ..TomlTrust::default()
            },
            command: TomlCommand {
                run: Some("bun run".to_owned()),
                ..TomlCommand::default()
            },
            ..TomlConfig::default()
        };
        let cfg = merge(&blank_cli(), &t);
        assert_eq!(cfg.global.app, "myapp");
        assert_eq!(cfg.update.policy, Policy::Auto);
        assert_eq!(cfg.update.check_interval, 60);
        assert_eq!(
            cfg.update.manifest.as_deref(),
            Some("https://example.com/m.json")
        );
        assert_eq!(cfg.trust.trusted_keys, vec!["id:key".to_owned()]);
        assert_eq!(cfg.command.run.as_deref(), Some("bun run"));
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn cli_overrides_toml() {
        // The CLI/env slot (clap folds env into these same fields) wins over TOML;
        // CLI-over-env within the slot is clap's own contract.
        let t = TomlConfig {
            global: TomlGlobal {
                app: Some("from_toml".to_owned()),
                ..TomlGlobal::default()
            },
            update: TomlUpdate {
                policy: Some(Policy::Auto),
                check_interval: Some(60),
                ..TomlUpdate::default()
            },
            ..TomlConfig::default()
        };

        let mut cli = blank_cli();
        cli.policy = Some(Policy::Off);
        cli.interval = Some(9);
        cli.app = Some("from_cli".to_owned());

        let cfg = merge(&cli, &t);
        assert_eq!(cfg.update.policy, Policy::Off);
        assert_eq!(cfg.update.check_interval, 9);
        assert_eq!(cfg.global.app, "from_cli");
    }

    #[test]
    fn parses_partial_toml_with_defaulted_sections() {
        // Missing sections and missing keys must default rather than error, so a
        // sparse lode.toml is valid.
        let parsed: TomlConfig =
            toml::from_str("[update]\npolicy = \"auto\"\nmanifest = \"https://x/m.json\"\n")
                .unwrap();
        let cfg = merge(&blank_cli(), &parsed);
        assert_eq!(cfg.update.policy, Policy::Auto);
        assert_eq!(cfg.update.manifest.as_deref(), Some("https://x/m.json"));
        assert_eq!(cfg.global.app, "app"); // default, no [global] table present
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn parses_example_toml() {
        // The shipped full reference must round-trip through the parser + merge.
        let text = include_str!("../docs/lode.example.toml");
        let parsed: TomlConfig = toml::from_str(text).unwrap();
        let cfg = merge(&blank_cli(), &parsed);
        assert_eq!(cfg.global.app, "myapp");
        assert_eq!(cfg.update.policy, Policy::Check);
        assert_eq!(cfg.command.run.as_deref(), Some("bun run app.js"));
        assert_eq!(cfg.command.exec.as_deref(), Some("bun"));
        assert_eq!(cfg.supervise.restart_mode, RestartMode::StopStart);
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn parses_starter_toml() {
        // The minimal scaffolded starter must parse + validate against the structs.
        let parsed: TomlConfig = toml::from_str(STARTER_TOML).unwrap();
        let cfg = merge(&blank_cli(), &parsed);
        assert_eq!(cfg.global.app, "myapp");
        assert_eq!(cfg.command.run.as_deref(), Some("./myapp"));
        assert_eq!(cfg.command.exec.as_deref(), Some("./myapp"));
        assert!(cfg.update.manifest.is_some());
        assert_eq!(
            cfg.update.asset.as_deref(),
            Some("myapp-linux-x86_64.tar.gz")
        );
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn parses_toml_without_command_section() {
        // A lode.toml with no [command] at all is valid: a manifest asset may
        // supply the launch command, so the gap is resolved at launch time.
        let parsed: TomlConfig =
            toml::from_str("[update]\nmanifest = \"https://x/m.json\"\n").unwrap();
        let cfg = merge(&blank_cli(), &parsed);
        assert_eq!(cfg.command.run, None);
        assert_eq!(cfg.command.exec, None);
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn seed_run_command_mirrors_landing_rules() {
        // raw keeps the filename; .gz drops the suffix; archives fall back to the
        // `./{app}` convention (operator edits the scaffold otherwise).
        assert_eq!(
            seed_run_command(Path::new("/x/myapp.bin"), "app"),
            "./myapp.bin"
        );
        assert_eq!(seed_run_command(Path::new("/x/tool.gz"), "app"), "./tool");
        assert_eq!(
            seed_run_command(Path::new("/x/rel.tar.gz"), "myapp"),
            "./myapp"
        );
        assert_eq!(
            seed_run_command(Path::new("/x/rel.zip"), "myapp"),
            "./myapp"
        );
    }

    #[test]
    fn unknown_toml_key_rejected_and_named() {
        // An unknown key (e.g. a typo) must be a hard parse error naming the key
        // (operators fix lode.toml while the app is paused — a silent no-op strands them).
        let err = toml::from_str::<TomlConfig>("[update]\nbogus_key = \"stable\"\n").unwrap_err();
        let rendered = crate::error::Error::from(err).to_string();
        assert!(rendered.contains("bogus_key"), "got: {rendered}");

        // ...in every section, including the top level.
        assert!(toml::from_str::<TomlConfig>("[globall]\napp = \"x\"\n").is_err());
        assert!(toml::from_str::<TomlConfig>("[supervise]\nrestart_maxx = 3\n").is_err());

        // `[env]` stays open: arbitrary user-defined variable names are the point.
        let parsed: TomlConfig = toml::from_str("[env]\nMY_CUSTOM_VAR = \"1\"\n").unwrap();
        assert_eq!(
            parsed.env.get("MY_CUSTOM_VAR").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn cli_log_level_overrides_toml() {
        let t = TomlConfig {
            global: TomlGlobal {
                log_level: Some("debug".to_owned()),
                ..TomlGlobal::default()
            },
            ..TomlConfig::default()
        };
        // TOML supplies the level when the CLI/env slot is unset…
        assert_eq!(merge(&blank_cli(), &t).global.log_level, "debug");
        // …and an explicit CLI/env value wins over it.
        let mut cli = blank_cli();
        cli.log_level = Some("warn".to_owned());
        assert_eq!(merge(&cli, &t).global.log_level, "warn");
    }

    #[test]
    fn peek_log_level_reads_toml_leniently() {
        let dir = std::env::temp_dir().join(format!("lode-peek-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lode.toml");
        let cli_for = |p: &Path| {
            let mut cli = blank_cli();
            cli.config = Some(p.to_string_lossy().into_owned());
            cli
        };

        // A config file with a level → Some(level).
        std::fs::write(&path, "[global]\nlog_level = \"debug\"\n").unwrap();
        assert_eq!(peek_log_level(&cli_for(&path)).as_deref(), Some("debug"));

        // Present but without the key → None.
        std::fs::write(&path, "[global]\napp = \"x\"\n").unwrap();
        assert_eq!(peek_log_level(&cli_for(&path)), None);

        // Malformed → None (full resolve reports the error later, logging up).
        std::fs::write(&path, "[global\nlog_level = ").unwrap();
        assert_eq!(peek_log_level(&cli_for(&path)), None);

        // Absent → None.
        assert_eq!(peek_log_level(&cli_for(&dir.join("missing.toml"))), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_key_fields_rejected() {
        // app
        let mut cli = blank_cli();
        cli.app = Some(String::new());
        let err = validate(&merge(&cli, &TomlConfig::default())).unwrap_err();
        assert!(err.to_string().contains("global.app"), "got: {err}");

        // channel
        let mut cli = blank_cli();
        cli.channel = Some("  ".to_owned());
        let err = validate(&merge(&cli, &TomlConfig::default())).unwrap_err();
        assert!(err.to_string().contains("update.channel"), "got: {err}");

        // asset (only when set — None stays fine)
        let mut cli = blank_cli();
        cli.asset = Some(String::new());
        let err = validate(&merge(&cli, &TomlConfig::default())).unwrap_err();
        assert!(err.to_string().contains("update.asset"), "got: {err}");

        // run/exec are deliberately NOT validated here: an empty/unset command is
        // resolved (or clearly rejected) at launch, where the manifest override is
        // known — see supervisor::effective_command.
        let mut cli = blank_cli();
        cli.run = Some(String::new());
        assert!(validate(&merge(&cli, &TomlConfig::default())).is_ok());
    }

    #[test]
    fn manifest_github_xor_rejected() {
        let mut cli = blank_cli();
        cli.manifest = Some("https://example.com/m.json".to_owned());
        cli.github = Some("owner/name".to_owned());
        let cfg = merge(&cli, &TomlConfig::default());
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn backoff_range_rejected() {
        let mut cli = blank_cli();
        cli.restart_backoff = Some(60);
        cli.restart_backoff_max = Some(30);
        let cfg = merge(&cli, &TomlConfig::default());
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn split_csv_trims_and_drops_empties() {
        assert_eq!(split_csv(" a , b ,, c "), vec!["a", "b", "c"]);
        assert!(split_csv("").is_empty());
    }
}
