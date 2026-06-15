//! Publisher / operator authoring helpers, exposed under the `lode-cli` name
//! (a symlink to the `lode` binary; see [`crate::run`]). The loader binary itself
//! has no subcommands — these live here so packaging, signing and manifest
//! authoring stay out of the loader's passthrough namespace.
//!
//! Crypto reuses the runtime primitives in [`crate::verify`] so the canonical
//! sign message and `key_id` always match what the loader enforces.

use std::fs;
use std::io::{self, Write as _};
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::{Map, Value, json};

use crate::verify::{
    Artifact, artifact_message, decode_key, key_id, sha256_hex_file, verify_signature,
};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// `keygen` — generate an ed25519 keypair and print (and optionally write) it.
pub(crate) fn keygen(out: Option<&str>) -> Result<()> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| anyhow::anyhow!("getrandom failed: {e}"))?;
    let signing = SigningKey::from_bytes(&seed);
    let public = signing.verifying_key().to_bytes();
    let id = key_id(&public);
    let private_b64 = B64.encode(seed);
    let public_b64 = B64.encode(public);

    if let Some(prefix) = out {
        let key_path = format!("{prefix}.key");
        write_private(&key_path, &private_b64).with_context(|| format!("write {key_path}"))?;
        fs::write(format!("{prefix}.pub"), format!("{id} {public_b64}\n"))
            .with_context(|| format!("write {prefix}.pub"))?;
    }

    let mut stdout = io::stdout().lock();
    writeln!(stdout, "key_id:       {id}")?;
    writeln!(stdout, "public:      {public_b64}")?;
    writeln!(stdout, "trustedKeys: {id}:{public_b64}")?;
    writeln!(
        stdout,
        "private:     {private_b64}   # keep secret — never commit"
    )?;
    Ok(())
}

/// Write a PRIVATE key file owner-only (0600). `mode` on `OpenOptions` only
/// applies when the file is created, so permissions are also tightened after the
/// write — re-running keygen over an existing world-readable key fixes it up.
#[cfg(unix)]
fn write_private(path: &str, contents: &str) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn write_private(path: &str, contents: &str) -> io::Result<()> {
    fs::write(path, contents)
}

/// The filename component of a path — the asset `name` that the §1 signature binds.
fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

/// Load a base64-encoded ed25519 private seed from a key file (`--key`).
fn seed_from_file(key_path: &str) -> Result<[u8; 32]> {
    let private_b64 =
        fs::read_to_string(key_path).with_context(|| format!("read key {key_path}"))?;
    decode_key(&private_b64).context("decode private key")
}

/// Load a base64-encoded ed25519 private seed from an environment variable
/// (`--key-env`, e.g. a CI secret); the key never touches disk.
fn seed_from_env(env_name: &str) -> Result<[u8; 32]> {
    let private_b64 = std::env::var(env_name)
        .map_err(|_| anyhow::anyhow!("signing key env var {env_name} is not set"))?;
    decode_key(&private_b64).with_context(|| format!("decode private key from ${env_name}"))
}

/// Resolve the signing seed from EXACTLY ONE of `--key` (a file) or `--key-env`
/// (an env var). Erroring clearly when neither or both is supplied.
fn resolve_sign_key(key: Option<&str>, key_env: Option<&str>) -> Result<[u8; 32]> {
    match (key, key_env) {
        (Some(path), None) => seed_from_file(path),
        (None, Some(env)) => seed_from_env(env),
        (Some(_), Some(_)) => bail!("pass exactly one of --key or --key-env, not both"),
        (None, None) => bail!("a signing key is required: pass --key <path> or --key-env <ENV>"),
    }
}

/// Sign an asset with `seed`: return `(sha256, sig_b64, key_id)` over the §1
/// canonical message (which binds the optional `run`/`exec` launch overrides).
fn sign_artifact(a: &Artifact<'_>, seed: &[u8; 32]) -> Result<(String, String, String)> {
    let signing = SigningKey::from_bytes(seed);
    let id = key_id(&signing.verifying_key().to_bytes());
    let sha256 = sha256_hex_file(Path::new(a.path))?;
    let message = artifact_message(a, &sha256);
    let sig_b64 = B64.encode(signing.sign(&message).to_bytes());
    Ok((sha256, sig_b64, id))
}

/// `sign` — compute sha256 + signature for an asset and print them. The signing key
/// comes from exactly one of `--key` (file) or `--key-env` (env var, for CI). The
/// signature (§1) binds the asset filename, the version, the digest and the
/// optional `--run`/`--exec` launch overrides (which must then be published
/// verbatim in the manifest asset); it is exactly the string a publisher uploads
/// as the GitHub asset `label`.
pub(crate) fn sign(
    artifact: &str,
    version: &str,
    run: Option<&str>,
    exec: Option<&str>,
    key: Option<&str>,
    key_env: Option<&str>,
) -> Result<()> {
    validate_overrides(run, exec)?;
    let seed = resolve_sign_key(key, key_env)?;
    let name = basename(artifact);
    let a = Artifact {
        name,
        version,
        path: artifact,
        run,
        exec,
    };
    let (sha256, sig_b64, id) = sign_artifact(&a, &seed)?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "sha256: {sha256}")?;
    writeln!(stdout, "sig:    {sig_b64}")?;
    writeln!(stdout, "key_id: {id}")?;
    Ok(())
}

/// Reject malformed `--run`/`--exec` overrides up front, with the same rule the
/// loader applies at manifest parse — so a publisher cannot sign a value the
/// loader will refuse to load.
fn validate_overrides(run: Option<&str>, exec: Option<&str>) -> Result<()> {
    if let Some(run) = run {
        crate::manifest::validate_command_override("run", run)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    if let Some(exec) = exec {
        crate::manifest::validate_command_override("exec", exec)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(())
}

/// `verify` — recompute sha256 and check the signature locally. `run`/`exec` must
/// match the published overrides (they are part of the signed message).
pub(crate) fn verify(
    artifact: &str,
    version: &str,
    run: Option<&str>,
    exec: Option<&str>,
    public_b64: &str,
    sig_b64: &str,
) -> Result<()> {
    let name = basename(artifact);
    let a = Artifact {
        name,
        version,
        path: artifact,
        run,
        exec,
    };
    let sha256 = sha256_hex_file(Path::new(a.path))?;
    let message = artifact_message(&a, &sha256);
    let ok = verify_signature(public_b64, &message, sig_b64)?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "sha256: {sha256}")?;
    if ok {
        writeln!(stdout, "signature: OK")?;
        Ok(())
    } else {
        bail!("signature: FAILED");
    }
}

/// `manifest` — sign an asset and emit (or create-or-merge) a `lode/v1` manifest
/// entry. The asset is keyed by its filename (`name` = basename of `artifact`);
/// the optional `run`/`exec` launch overrides ARE part of the signature (they
/// steer what the loader executes), while `url`/`size` are runtime fields and are
/// NOT. Without `into` the single-asset manifest is printed to stdout; with `into`
/// the asset is upserted (by `name`) into `versions[version].assets` and
/// `channels[channel].latest` is set to `version`. `app` is the manifest top-level
/// `name` (from `--app`/`LODE_APP_NAME`); it is preserved when merging.
#[allow(clippy::too_many_arguments)]
pub(crate) fn manifest(
    app: &str,
    artifact: &str,
    version: &str,
    url: &str,
    run: Option<&str>,
    exec: Option<&str>,
    size: Option<u64>,
    channel: &str,
    key_path: &str,
    into: Option<&str>,
) -> Result<()> {
    validate_overrides(run, exec)?;
    let name = basename(artifact);
    let a = Artifact {
        name,
        version,
        path: artifact,
        run,
        exec,
    };
    let seed = seed_from_file(key_path)?;
    let (sha256, sig_b64, id) = sign_artifact(&a, &seed)?;

    // The asset object: name/sha256/sig/key_id/run/exec are the signed identity +
    // digest + launch overrides; url/size are runtime fields (never signed).
    // Format is derived from the filename at install time, so it is not stored.
    let mut asset = Map::new();
    asset.insert("name".to_owned(), json!(name));
    asset.insert("url".to_owned(), json!(url));
    asset.insert("sha256".to_owned(), json!(sha256));
    asset.insert("sig".to_owned(), json!(sig_b64));
    asset.insert("key_id".to_owned(), json!(id));
    if let Some(r) = run {
        asset.insert("run".to_owned(), json!(r));
    }
    if let Some(x) = exec {
        asset.insert("exec".to_owned(), json!(x));
    }
    if let Some(s) = size {
        asset.insert("size".to_owned(), json!(s));
    }
    let asset_obj = Value::Object(asset);

    let Some(path) = into else {
        // Print a complete one-asset manifest.
        let man = json!({
            "schema": "lode/v1", "name": app, "key_id": id,
            "channels": { channel: { "latest": version } },
            "versions": { version: { "assets": [asset_obj] } },
        });
        writeln!(
            io::stdout().lock(),
            "{}",
            serde_json::to_string_pretty(&man)?
        )?;
        return Ok(());
    };

    // Create-or-merge into an existing manifest.json.
    let mut root: Value = if Path::new(path).exists() {
        serde_json::from_str(&fs::read_to_string(path).with_context(|| format!("read {path}"))?)
            .with_context(|| format!("parse {path} as JSON"))?
    } else {
        json!({ "schema": "lode/v1", "name": app, "key_id": id, "channels": {}, "versions": {} })
    };
    let obj = root
        .as_object_mut()
        .context("manifest root is not a JSON object")?;
    obj.entry("schema").or_insert_with(|| json!("lode/v1"));
    obj.entry("name").or_insert_with(|| json!(app));
    obj.entry("key_id").or_insert_with(|| json!(id));

    // versions[version].assets: replace any same-name entry, then append.
    let versions = obj
        .entry("versions")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`versions` is not an object")?;
    let ver = versions
        .entry(version)
        .or_insert_with(|| json!({ "assets": [] }))
        .as_object_mut()
        .context("version entry is not an object")?;
    let assets = ver
        .entry("assets")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("`assets` is not an array")?;
    assets.retain(|x| x.get("name").and_then(Value::as_str) != Some(name));
    assets.push(asset_obj);

    // channels[channel].latest = version
    let channels = obj
        .entry("channels")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`channels` is not an object")?;
    let mut chan = Map::new();
    chan.insert("latest".to_owned(), json!(version));
    channels.insert(channel.to_owned(), Value::Object(chan));

    fs::write(path, format!("{}\n", serde_json::to_string_pretty(&root)?))
        .with_context(|| format!("write {path}"))?;
    writeln!(
        io::stdout().lock(),
        "updated {path}: {name} {version} -> channel {channel}"
    )?;
    Ok(())
}

/// `manifest-sign` — sign a complete `lode/v1` manifest in place. Loads
/// `into`, computes the top-level ed25519 signature over the canonical manifest
/// message (binding `name` + `key_id` + the channel/version catalog, EXCLUDING the
/// `sig` field), and writes the signer's `key_id` + `sig` back into the file.
///
/// The loader verifies this under `[trust].require_signature` (see
/// [`crate::install::verify_manifest_identity`]); both sides build the signed bytes
/// via [`crate::manifest::Manifest::signing_message`], so they always agree.
pub(crate) fn manifest_sign(into: &str, key_path: &str) -> Result<()> {
    let bytes = fs::read(into).with_context(|| format!("read {into}"))?;
    let mut manifest =
        crate::manifest::parse(&bytes).map_err(|e| anyhow::anyhow!("parse {into}: {e}"))?;

    let private_b64 =
        fs::read_to_string(key_path).with_context(|| format!("read key {key_path}"))?;
    let seed = decode_key(&private_b64).context("decode private key")?;
    let signing = SigningKey::from_bytes(&seed);
    let id = key_id(&signing.verifying_key().to_bytes());

    // `key_id` is part of the signed message, so stamp it before building the bytes.
    manifest.key_id = Some(id.clone());
    let message = manifest.signing_message();
    let sig_b64 = B64.encode(signing.sign(&message).to_bytes());

    // Write `key_id` + `sig` into the JSON, preserving everything else verbatim.
    let mut root: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {into} as JSON"))?;
    let obj = root
        .as_object_mut()
        .context("manifest root is not a JSON object")?;
    obj.insert("key_id".to_owned(), json!(id));
    obj.insert("sig".to_owned(), json!(sig_b64));

    fs::write(into, format!("{}\n", serde_json::to_string_pretty(&root)?))
        .with_context(|| format!("write {into}"))?;
    writeln!(io::stdout().lock(), "signed {into}: key_id {id}")?;
    Ok(())
}

/// `init` — write the minimal starter `lode.toml` (the full documented reference
/// lives in `docs/lode.example.toml`). Shares the same template lode scaffolds on
/// first run ([`crate::config::STARTER_TOML`]).
pub(crate) fn init(path: Option<&str>) -> Result<()> {
    let template = crate::config::STARTER_TOML;
    match path {
        Some(p) => {
            if Path::new(p).exists() {
                bail!("{p} already exists — refusing to overwrite");
            }
            fs::write(p, template).with_context(|| format!("write {p}"))?;
            writeln!(io::stdout().lock(), "wrote {p}")?;
        }
        None => write!(io::stdout().lock(), "{template}")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--key`/`--key-env` are mutually exclusive and one is required.
    #[test]
    fn resolve_sign_key_requires_exactly_one() {
        // Neither source → error.
        assert!(resolve_sign_key(None, None).is_err());
        // Both sources → error.
        assert!(resolve_sign_key(Some("/some.key"), Some("LODE_SIGNING_KEY")).is_err());
    }

    /// `--key` reads + decodes a base64 seed file (same path the env var feeds).
    #[test]
    fn resolve_sign_key_reads_file_seed() {
        let seed = [7u8; 32];
        let dir = std::env::temp_dir().join(format!("lode-authoring-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("priv.key");
        fs::write(&key_path, B64.encode(seed)).unwrap();

        let got = resolve_sign_key(Some(key_path.to_str().unwrap()), None).unwrap();
        assert_eq!(got, seed);
        let _ = fs::remove_dir_all(&dir);
    }

    /// `--key-env` pointing at an unset variable errors clearly (it never reads a
    /// file). The set-and-decode path mirrors the file path through `decode_key`.
    #[test]
    fn key_env_unset_errors() {
        assert!(seed_from_env("LODE_TEST_UNSET_SIGNING_KEY_VAR_XYZ").is_err());
        assert!(resolve_sign_key(None, Some("LODE_TEST_UNSET_SIGNING_KEY_VAR_XYZ")).is_err());
    }

    /// `keygen --out` writes the PRIVATE key owner-only (0600) — both on first
    /// creation and when re-run over an existing world-readable key file.
    #[cfg(unix)]
    #[test]
    fn keygen_private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("lode-keygen-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let prefix = dir.join("id");
        let prefix = prefix.to_str().unwrap();
        let key_path = dir.join("id.key");

        keygen(Some(prefix)).unwrap();
        let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Re-running over a pre-existing 0644 key tightens it back to 0600.
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
        keygen(Some(prefix)).unwrap();
        let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = fs::remove_dir_all(&dir);
    }
}
