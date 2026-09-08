//! `lode-cli self-update` — replace THIS lode binary with a release of lode
//! itself.
//!
//! lode's own update source is fixed — its GitHub releases — and is never the
//! app's `lode.toml` / `LODE_*` source: the command builds a self-contained
//! [`Config`] (`[update].github = dotns/lode`, the release asset for this
//! platform, `require_signature = enforce` against the release keys compiled
//! into the binary) and then reuses the ordinary chain — GitHub adapter →
//! download → sha256 + signature gate → unpack — that installs any app version.
//! The verified binary is staged beside the running executable, probed
//! (`--version` must report the target), and atomically renamed over it. The
//! running process keeps its old inode, so nothing changes until lode is next
//! started; a running supervisor is never disturbed.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, Config, Overrides, Policy, RequireSignature};
use crate::error::{Error, Result};
use crate::{download, engine, install, manifest};

/// lode's own release repository (`owner/name`).
pub const RELEASE_REPO: &str = "dotns/lode";
/// The app name the self-contained config runs under (the GitHub adapter names
/// its synthesised catalog after it; nothing on disk is keyed by it).
const APP: &str = "lode";
/// The archive member that is the lode binary (`lode-cli` is a symlink to it).
const BINARY: &str = "lode";

/// Run `self-update`: install the newest stable lode release — or the release
/// `tag` — over the running executable.
///
/// `trusted_keys` are the release-key entries the download must be signed by;
/// `current_version` is this binary's version (the "already up to date"
/// reference and the downgrade guard).
pub fn run(tag: Option<&str>, trusted_keys: &[String], current_version: &str) -> Result<()> {
    if trusted_keys.is_empty() {
        return Err(Error::Config(
            "no lode release key is compiled into this binary; pass --release-key <entry>"
                .to_owned(),
        ));
    }
    let exe = current_exe()?;
    let dir = scratch_dir()?;
    let cfg = config(RELEASE_REPO, &release_asset()?, tag, trusted_keys, &dir)?;
    let result = run_with(&cfg, &exe, current_version);
    let _ = fs::remove_dir_all(&dir);
    result
}

/// The self-contained config a self-update runs under: GitHub source `repo`,
/// the release `asset` for this host, `tag` as the pin when given, `enforce`
/// against `trusted_keys`, and `dir` as the download scratch space. Built from
/// an [`Overrides`] layer with no `lode.toml`, so nothing the operator
/// configured for the app leaks in.
fn config(
    repo: &str,
    asset: &str,
    tag: Option<&str>,
    trusted_keys: &[String],
    dir: &Path,
) -> Result<Config> {
    let dir = dir
        .to_str()
        .ok_or_else(|| Error::Config(format!("scratch dir {} is not UTF-8", dir.display())))?;
    let overrides = Overrides {
        app: Some(APP.to_owned()),
        dir: Some(dir.to_owned()),
        github: Some(repo.to_owned()),
        asset: Some(asset.to_owned()),
        pin: tag.map(ToOwned::to_owned),
        policy: Some(Policy::Off),
        require_signature: Some(RequireSignature::Enforce),
        trusted_keys: Some(trusted_keys.join(",")),
        ..Overrides::default()
    };
    config::resolve_with(&overrides, None)
}

/// The release asset built for this host — `lode-<os>-<arch>.tar.gz` as
/// `.github/workflows/release.yml` names it.
fn release_asset() -> Result<String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    asset_for(os, arch)
        .ok_or_else(|| Error::Config(format!("no prebuilt lode release for {os}/{arch}")))
}

/// `lode-<os>-<arch>.tar.gz` for a Rust `target_os`/`target_arch` pair, using the
/// release workflow's names (`darwin`/`linux`, `x64`/`arm64`); `None` when no
/// release is built for it.
fn asset_for(os: &str, arch: &str) -> Option<String> {
    let os = match os {
        "linux" => "linux",
        "macos" => "darwin",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some(format!("lode-{os}-{arch}.tar.gz"))
}

/// The update proper, against `cfg` and the executable at `exe` (split from
/// [`run`] so tests can drive it with a stub source and a scratch executable).
fn run_with(cfg: &Config, exe: &Path, current_version: &str) -> Result<()> {
    let manifest = manifest::fetch(cfg)?;
    install::verify_manifest_identity(cfg, &manifest)?;
    // No floor: the plan below decides against the running binary's version, with
    // a self-update-specific message; a pinned tag is the operator's deliberate
    // choice (also the downgrade path) and is installed as is.
    let target = manifest::resolve_target(
        &manifest,
        &cfg.update.channel,
        cfg.update.pin.as_deref(),
        None,
        None,
    )?;
    let mut out = std::io::stdout().lock();
    if cfg.update.pin.is_none() {
        match plan(&target, current_version) {
            Plan::UpToDate => {
                writeln!(
                    out,
                    "lode self-update: {current_version} is already the newest release"
                )?;
                return Ok(());
            }
            Plan::Older => {
                return Err(Error::Manifest(format!(
                    "the newest release ({target}) is older than this binary \
                     ({current_version}); pass --version <tag> to install it deliberately"
                )));
            }
            Plan::Install => {}
        }
    }

    let entry = manifest::version_entry(&manifest, &target)?;
    let asset = manifest::select_asset(entry, engine::required_asset(cfg)?)?;
    let (artifact, sha256) =
        download::fetch_artifact(cfg, asset, &target, &manifest::allowed_hosts(cfg))?;
    install::verify_download(cfg, &target, asset, &sha256)?;

    let unpack = cfg.global.dir.join("unpack");
    fs::create_dir_all(&unpack)?;
    install::extract(
        asset,
        manifest::format_from_name(&asset.name),
        &artifact,
        &unpack,
    )?;
    let binary = unpack.join(BINARY);
    if !binary.is_file() {
        return Err(Error::Install(format!(
            "{} does not contain a `{BINARY}` binary",
            asset.name
        )));
    }
    replace_executable(exe, &binary, &target)?;

    writeln!(
        out,
        "lode self-update: installed {target} at {} (was {current_version})",
        exe.display()
    )?;
    writeln!(
        out,
        "  restart lode to run it — a running supervisor keeps the old binary until then"
    )?;
    Ok(())
}

/// What following the channel `latest` should do relative to the running binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// `latest` is the running version.
    UpToDate,
    /// `latest` is older than the running version — refused unless a tag is named.
    Older,
    /// Newer (or not semver-comparable and different) — install it.
    Install,
}

/// Compare `target` (the channel latest) with `current` by semver precedence;
/// ids that don't parse fall back to plain equality.
fn plan(target: &str, current: &str) -> Plan {
    match (
        semver::Version::parse(target),
        semver::Version::parse(current),
    ) {
        (Ok(t), Ok(c)) => match t.cmp(&c) {
            Ordering::Equal => Plan::UpToDate,
            Ordering::Less => Plan::Older,
            Ordering::Greater => Plan::Install,
        },
        _ if target == current => Plan::UpToDate,
        _ => Plan::Install,
    }
}

/// Swap `binary` (the verified, unpacked release) in over `exe`: copy it beside
/// `exe` (same filesystem, so the final step is an atomic `rename`), carry over
/// `exe`'s permission bits, prove it runs and reports `version`, then rename it
/// over `exe`. On any failure the staged copy is removed and `exe` is untouched.
/// A running `exe` keeps its old inode (Unix never lets a mapped executable be
/// written — `ETXTBSY`), which is why this is a rename, not a write.
fn replace_executable(exe: &Path, binary: &Path, version: &str) -> Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| Error::Install(format!("{} has no parent directory", exe.display())))?;
    let name = exe
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| Error::Install(format!("{} has no usable filename", exe.display())))?;
    let staged = dir.join(format!(".{name}.self-update-{}", std::process::id()));
    fs::copy(binary, &staged)
        .map_err(|e| Error::Install(format!("stage {}: {e}", staged.display())))?;
    let result = install_staged(exe, &staged, version);
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

/// The steps after staging (see [`replace_executable`]), separated so a failure
/// in any of them cleans up the staged copy in one place.
fn install_staged(exe: &Path, staged: &Path, version: &str) -> Result<()> {
    let perms = fs::metadata(exe)?.permissions();
    fs::set_permissions(staged, perms)?;
    probe_version(staged, version)?;
    fs::rename(staged, exe).map_err(|e| Error::Install(format!("replace {}: {e}", exe.display())))
}

/// Run `staged --version` and require it to report `version`: catches a download
/// built for another platform (fails to exec) or a mislabelled release before it
/// replaces a working binary.
fn probe_version(staged: &Path, version: &str) -> Result<()> {
    let output = Command::new(staged)
        .arg("--version")
        .output()
        .map_err(|e| {
            Error::Install(format!(
                "run {} --version: {e} (is the release built for this platform?)",
                staged.display()
            ))
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && stdout.split_whitespace().any(|word| word == version) {
        return Ok(());
    }
    Err(Error::Install(format!(
        "downloaded lode reports {:?}, expected version {version}",
        stdout.trim()
    )))
}

/// The running executable, symlinks resolved — so a run via the `lode-cli`
/// symlink replaces the `lode` file it points at, never the link.
fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::Install(format!("locate the running executable: {e}")))?;
    exe.canonicalize()
        .map_err(|e| Error::Install(format!("resolve {}: {e}", exe.display())))
}

/// A per-process scratch directory under the system temp dir for the download
/// cache and the unpacked archive; [`run`] removes it when done.
fn scratch_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("lode-self-update-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use ed25519_dalek::{Signer as _, SigningKey};
    use serde_json::json;

    use super::*;
    use crate::stub::Stub;
    use crate::verify::{Artifact, artifact_message, key_id, sha256_hex};

    const B64: base64::engine::general_purpose::GeneralPurpose =
        base64::engine::general_purpose::STANDARD;
    const ASSET: &str = "lode-linux-x64.tar.gz";
    const LATEST: &str = "/repos/dotns/lode/releases/latest";

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lode-self-update-test-{}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn asset_for_maps_the_released_platforms_only() {
        assert_eq!(
            asset_for("linux", "x86_64").as_deref(),
            Some("lode-linux-x64.tar.gz")
        );
        assert_eq!(
            asset_for("linux", "aarch64").as_deref(),
            Some("lode-linux-arm64.tar.gz")
        );
        assert_eq!(
            asset_for("macos", "x86_64").as_deref(),
            Some("lode-darwin-x64.tar.gz")
        );
        assert_eq!(
            asset_for("macos", "aarch64").as_deref(),
            Some("lode-darwin-arm64.tar.gz")
        );
        assert!(asset_for("windows", "x86_64").is_none());
        assert!(asset_for("linux", "riscv64").is_none());
    }

    #[test]
    fn plan_compares_by_semver_then_equality() {
        assert_eq!(plan("0.3.0", "0.2.0"), Plan::Install);
        assert_eq!(plan("0.2.0", "0.2.0"), Plan::UpToDate);
        assert_eq!(plan("0.1.9", "0.2.0"), Plan::Older);
        assert_eq!(plan("0.3.0-rc.1", "0.2.0"), Plan::Install);
        // Not semver: only literal equality counts as up to date.
        assert_eq!(plan("nightly", "nightly"), Plan::UpToDate);
        assert_eq!(plan("nightly", "0.2.0"), Plan::Install);
    }

    #[test]
    fn config_is_self_contained_and_enforcing() {
        let dir = scratch("config");
        let keys = vec!["k1:AAAA".to_owned(), "k2:BBBB".to_owned()];
        let cfg = config("o/r", ASSET, Some("v1.2.3"), &keys, &dir).unwrap();
        assert_eq!(cfg.global.app, APP);
        assert_eq!(cfg.global.dir, dir);
        assert_eq!(cfg.update.github.as_deref(), Some("o/r"));
        assert!(cfg.update.manifest.is_none());
        assert_eq!(cfg.update.asset.as_deref(), Some(ASSET));
        assert_eq!(cfg.update.pin.as_deref(), Some("v1.2.3"));
        assert_eq!(cfg.trust.require_signature, RequireSignature::Enforce);
        assert_eq!(cfg.trust.trusted_keys, keys);
        assert!(cfg.trust.trusted_keys_file.is_none());
        assert!(cfg.http.headers.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    // --- the executable swap + the end-to-end flow need `/bin/sh` -------------

    /// A stand-in `lode`: a shell script whose `--version` prints `lode <version>`.
    #[cfg(unix)]
    fn write_fake_lode(path: &Path, version: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::write(path, fake_lode(version)).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn fake_lode(version: &str) -> String {
        format!("#!/bin/sh\necho \"lode {version}\"\n")
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn reported_version(exe: &Path) -> String {
        let out = Command::new(exe).arg("--version").output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// No `.<name>.self-update-*` staging file was left behind in `dir`.
    fn no_staging_leftovers(dir: &Path) -> bool {
        !fs::read_dir(dir).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".self-update-")
        })
    }

    #[cfg(unix)]
    #[test]
    fn replace_executable_swaps_in_the_probed_binary_and_keeps_its_mode() {
        let dir = scratch("swap");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "1.0.0", 0o750);
        let new = dir.join("unpacked-lode");
        write_fake_lode(&new, "9.9.9", 0o755);

        replace_executable(&exe, &new, "9.9.9").unwrap();

        assert_eq!(reported_version(&exe), "lode 9.9.9");
        assert_eq!(mode_of(&exe), 0o750, "the installed mode is carried over");
        assert!(no_staging_leftovers(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn replace_executable_keeps_the_old_binary_when_the_probe_fails() {
        let dir = scratch("probe-fail");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "1.0.0", 0o755);
        // Reports the wrong version (a mislabelled release)...
        let wrong = dir.join("wrong-lode");
        write_fake_lode(&wrong, "1.2.3", 0o755);
        let err = replace_executable(&exe, &wrong, "9.9.9").unwrap_err();
        assert!(err.to_string().contains("expected version 9.9.9"), "{err}");
        // ...and one that cannot run at all (a foreign-platform binary).
        let broken = dir.join("broken-lode");
        fs::write(&broken, b"\x7fELF not really").unwrap();
        let err = replace_executable(&exe, &broken, "9.9.9").unwrap_err();
        assert!(err.to_string().contains("--version"), "{err}");

        assert_eq!(reported_version(&exe), "lode 1.0.0");
        assert!(no_staging_leftovers(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A release tarball holding `lode` (a fake reporting `version`) plus the
    /// `lode-cli -> lode` symlink the real packaging ships.
    fn release_tarball(version: &str) -> Vec<u8> {
        use std::io::Write as _;
        let script = fake_lode(version);
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, BINARY, script.as_bytes())
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        link.set_cksum();
        builder.append_link(&mut link, "lode-cli", BINARY).unwrap();
        let tar_bytes = builder.into_inner().unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap()
    }

    /// Sign `bytes` as `ASSET` of `version` with a fresh ed25519 key: returns the
    /// sha256, the base64 signature and the matching trusted-key entry.
    fn sign(bytes: &[u8], version: &str) -> (String, String, String) {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).unwrap();
        let key = SigningKey::from_bytes(&seed);
        let sha = sha256_hex(bytes);
        let artifact = Artifact {
            name: ASSET,
            version,
            path: "",
            run: None,
            exec: None,
        };
        let sig = B64.encode(key.sign(&artifact_message(&artifact, &sha)).to_bytes());
        let public = key.verifying_key().to_bytes();
        let entry = format!("{}:{}", key_id(&public), B64.encode(public));
        (sha, sig, entry)
    }

    /// A stub GitHub whose `latest` and `tags/<tag>` both answer one release
    /// carrying `ASSET` (digest `sha`) and — when given — its `.sig` sidecar asset.
    fn github_stub(tag: &str, tarball: Vec<u8>, sha: &str, sig: Option<&str>) -> Stub {
        let tag = tag.to_owned();
        let sha = sha.to_owned();
        let sig = sig.map(ToOwned::to_owned);
        Stub::start(move |base| {
            let mut assets = vec![json!({
                "name": ASSET,
                "browser_download_url": format!("{base}/dl/{ASSET}"),
                "digest": format!("sha256:{sha}"),
            })];
            let mut routes = vec![(format!("/dl/{ASSET}"), tarball)];
            if let Some(sig) = sig {
                assets.push(json!({
                    "name": format!("{ASSET}.sig"),
                    "browser_download_url": format!("{base}/dl/{ASSET}.sig"),
                }));
                routes.push((format!("/dl/{ASSET}.sig"), format!("{sig}\n").into_bytes()));
            }
            let release = json!({ "tag_name": tag, "assets": assets })
                .to_string()
                .into_bytes();
            routes.push((LATEST.to_owned(), release.clone()));
            routes.push((format!("/repos/dotns/lode/releases/tags/{tag}"), release));
            routes
        })
    }

    /// The self-update config pointed at `stub` (loopback http is always allowed).
    fn stub_config(stub: &Stub, tag: Option<&str>, keys: &[String], dir: &Path) -> Config {
        let mut cfg = config(RELEASE_REPO, ASSET, tag, keys, dir).unwrap();
        cfg.update.github_api = stub.base.clone();
        cfg
    }

    #[cfg(unix)]
    #[test]
    fn installs_a_signed_release_over_the_running_binary() {
        let dir = scratch("install");
        let exe = dir.join("bin").join("lode");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        write_fake_lode(&exe, "0.1.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, entry) = sign(&tarball, "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));
        let cfg = stub_config(&stub, None, &[entry], &dir.join("scratch"));

        run_with(&cfg, &exe, "0.1.0").unwrap();

        assert_eq!(reported_version(&exe), "lode 9.9.9");
        assert!(no_staging_leftovers(exe.parent().unwrap()));
        // API → sidecar (during the manifest fetch) → the artifact itself.
        assert_eq!(
            stub.served(),
            vec![
                LATEST.to_owned(),
                format!("/dl/{ASSET}.sig"),
                format!("/dl/{ASSET}")
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_an_unsigned_release() {
        let dir = scratch("unsigned");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "0.1.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, _sig, entry) = sign(&tarball, "9.9.9");
        // No `.sig` sidecar asset and no label: unsigned under `enforce`.
        let stub = github_stub("v9.9.9", tarball, &sha, None);
        let cfg = stub_config(&stub, None, &[entry], &dir.join("scratch"));

        let err = run_with(&cfg, &exe, "0.1.0").unwrap_err();
        assert!(matches!(err, Error::Verify(_)), "{err}");
        assert_eq!(reported_version(&exe), "lode 0.1.0");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_release_signed_by_another_key() {
        let dir = scratch("wrong-key");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "0.1.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, _entry) = sign(&tarball, "9.9.9");
        let (_, _, other) = sign(b"another key", "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));
        let cfg = stub_config(&stub, None, &[other], &dir.join("scratch"));

        let err = run_with(&cfg, &exe, "0.1.0").unwrap_err();
        assert!(matches!(err, Error::Verify(_)), "{err}");
        assert_eq!(reported_version(&exe), "lode 0.1.0");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn is_a_no_op_when_already_on_the_newest_release() {
        let dir = scratch("up-to-date");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "9.9.9", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, entry) = sign(&tarball, "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));
        let cfg = stub_config(&stub, None, &[entry], &dir.join("scratch"));

        run_with(&cfg, &exe, "9.9.9").unwrap();

        assert_eq!(reported_version(&exe), "lode 9.9.9");
        assert!(!stub.served().iter().any(|p| p == &format!("/dl/{ASSET}")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_an_older_latest_unless_the_tag_is_named() {
        let dir = scratch("older");
        let exe = dir.join("lode");
        write_fake_lode(&exe, "10.0.0", 0o755);
        let tarball = release_tarball("9.9.9");
        let (sha, sig, entry) = sign(&tarball, "9.9.9");
        let stub = github_stub("v9.9.9", tarball, &sha, Some(&sig));

        // Following `latest` onto an older release is refused...
        let cfg = stub_config(
            &stub,
            None,
            std::slice::from_ref(&entry),
            &dir.join("scratch"),
        );
        let err = run_with(&cfg, &exe, "10.0.0").unwrap_err();
        assert!(err.to_string().contains("older than this binary"), "{err}");
        assert_eq!(reported_version(&exe), "lode 10.0.0");

        // ...while naming the raw tag installs it: the pin resolves to the same
        // `9.9.9` id the signature was made over, not the raw `v9.9.9`.
        let cfg = stub_config(&stub, Some("v9.9.9"), &[entry], &dir.join("scratch"));
        run_with(&cfg, &exe, "10.0.0").unwrap();
        assert_eq!(reported_version(&exe), "lode 9.9.9");
        assert!(
            stub.served()
                .iter()
                .any(|p| p == "/repos/dotns/lode/releases/tags/v9.9.9")
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
