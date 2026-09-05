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
use ed25519_dalek::Signer as _;
use serde_json::{Map, Value, json};

use lode_core::verify::{
    Algorithm, Artifact, PublicKey, artifact_message, sha256_hex_file, verify_signature,
};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// A publisher's private key, tagged with its algorithm (the secret half of a
/// [`PublicKey`]). On disk / in an env var it is `<alg>:<base64>` — bare base64 for
/// ed25519, so keys written before the algorithm was selectable keep working.
enum SecretKey {
    Ed25519(ed25519_dalek::SigningKey),
    EcdsaP256(p256::ecdsa::SigningKey),
    EcdsaP384(p384::ecdsa::SigningKey),
}

impl SecretKey {
    /// Draw a fresh key from the OS RNG.
    fn generate(alg: Algorithm) -> Result<Self> {
        let len = match alg {
            Algorithm::Ed25519 | Algorithm::EcdsaP256 => 32,
            Algorithm::EcdsaP384 => 48,
        };
        // ed25519 accepts any 32-byte seed; an ECDSA scalar must lie in [1, n), so
        // a draw is (with probability ~2^-128) rejected — retry rather than fail.
        for _ in 0..8 {
            let mut buf = [0u8; 48];
            getrandom::getrandom(&mut buf[..len])
                .map_err(|e| anyhow::anyhow!("getrandom failed: {e}"))?;
            if let Ok(key) = Self::decode(alg, &buf[..len]) {
                return Ok(key);
            }
        }
        bail!("could not derive a {alg} key from the OS RNG")
    }

    /// Decode raw secret bytes (an ed25519 seed or an ECDSA scalar) under `alg`.
    fn decode(alg: Algorithm, bytes: &[u8]) -> Result<Self> {
        Ok(match alg {
            Algorithm::Ed25519 => {
                let seed: &[u8; 32] = bytes.try_into().map_err(|_| {
                    anyhow::anyhow!("expected 32-byte ed25519 seed, got {} bytes", bytes.len())
                })?;
                Self::Ed25519(ed25519_dalek::SigningKey::from_bytes(seed))
            }
            Algorithm::EcdsaP256 => Self::EcdsaP256(
                p256::ecdsa::SigningKey::from_slice(bytes).context("invalid ecdsa-p256 key")?,
            ),
            Algorithm::EcdsaP384 => Self::EcdsaP384(
                p384::ecdsa::SigningKey::from_slice(bytes).context("invalid ecdsa-p384 key")?,
            ),
        })
    }

    /// Parse the key-file / env-var form: `<alg>:<base64>`, or bare base64 (ed25519).
    fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (alg, b64) = match text.split_once(':') {
            Some((alg, b64)) => (alg.parse::<Algorithm>()?, b64),
            None => (Algorithm::Ed25519, text),
        };
        let bytes = B64.decode(b64.trim()).context("base64 decode")?;
        Self::decode(alg, &bytes)
    }

    const fn algorithm(&self) -> Algorithm {
        match self {
            Self::Ed25519(_) => Algorithm::Ed25519,
            Self::EcdsaP256(_) => Algorithm::EcdsaP256,
            Self::EcdsaP384(_) => Algorithm::EcdsaP384,
        }
    }

    fn public(&self) -> PublicKey {
        match self {
            Self::Ed25519(k) => PublicKey::Ed25519(k.verifying_key()),
            Self::EcdsaP256(k) => PublicKey::EcdsaP256(*k.verifying_key()),
            Self::EcdsaP384(k) => PublicKey::EcdsaP384(*k.verifying_key()),
        }
    }

    /// The key-file / env-var form (see [`Self::parse`]).
    fn encode(&self) -> String {
        let b64 = match self {
            Self::Ed25519(k) => B64.encode(k.to_bytes()),
            Self::EcdsaP256(k) => B64.encode(k.to_bytes()),
            Self::EcdsaP384(k) => B64.encode(k.to_bytes()),
        };
        tagged(self.algorithm(), b64)
    }

    /// Raw signature bytes over `message` (64 bytes ed25519 / P-256, 96 bytes P-384).
    fn sign(&self, message: &[u8]) -> Vec<u8> {
        match self {
            Self::Ed25519(k) => k.sign(message).to_bytes().to_vec(),
            Self::EcdsaP256(k) => {
                let sig: p256::ecdsa::Signature = k.sign(message);
                sig.to_bytes().to_vec()
            }
            Self::EcdsaP384(k) => {
                let sig: p384::ecdsa::Signature = k.sign(message);
                sig.to_bytes().to_vec()
            }
        }
    }
}

/// Prefix `value` with `<alg>:` — except for ed25519, whose untagged form is what
/// every pre-existing key file, config and manifest already uses.
fn tagged(alg: Algorithm, value: String) -> String {
    if alg == Algorithm::Ed25519 {
        value
    } else {
        format!("{alg}:{value}")
    }
}

/// Set (or, for the ed25519 default, clear) the advisory `alg` on a manifest or
/// asset object, keeping it consistent with the `key_id` stamped beside it.
fn stamp_alg(obj: &mut Map<String, Value>, alg: Algorithm) {
    if alg == Algorithm::Ed25519 {
        obj.remove("alg");
    } else {
        obj.insert("alg".to_owned(), json!(alg.name()));
    }
}

/// `keygen` — generate a publisher keypair under `alg` and print (and optionally
/// write) it.
pub(crate) fn keygen(out: Option<&str>, alg: &str) -> Result<()> {
    let alg: Algorithm = alg.parse()?;
    let secret = SecretKey::generate(alg)?;
    let public = secret.public();
    let id = public.key_id();
    let private = secret.encode();
    let public_b64 = B64.encode(public.to_bytes());
    let trusted = tagged(alg, format!("{id}:{public_b64}"));

    if let Some(prefix) = out {
        let key_path = format!("{prefix}.key");
        write_private(&key_path, &private).with_context(|| format!("write {key_path}"))?;
        fs::write(
            format!("{prefix}.pub"),
            format!("{} {public_b64}\n", tagged(alg, id.clone())),
        )
        .with_context(|| format!("write {prefix}.pub"))?;
    }

    let mut stdout = io::stdout().lock();
    writeln!(stdout, "alg:         {alg}")?;
    writeln!(stdout, "key_id:       {id}")?;
    writeln!(stdout, "public:      {public_b64}")?;
    writeln!(stdout, "trustedKeys: {trusted}")?;
    writeln!(
        stdout,
        "private:     {private}   # keep secret — never commit"
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

/// Load a private key from a key file (`--key`; the form `keygen` writes).
fn key_from_file(key_path: &str) -> Result<SecretKey> {
    let text = fs::read_to_string(key_path).with_context(|| format!("read key {key_path}"))?;
    SecretKey::parse(&text).context("decode private key")
}

/// Load a private key from an environment variable (`--key-env`, e.g. a CI
/// secret); the key never touches disk.
fn key_from_env(env_name: &str) -> Result<SecretKey> {
    let text = std::env::var(env_name)
        .map_err(|_| anyhow::anyhow!("signing key env var {env_name} is not set"))?;
    SecretKey::parse(&text).with_context(|| format!("decode private key from ${env_name}"))
}

/// Resolve the signing key from EXACTLY ONE of `--key` (a file) or `--key-env`
/// (an env var). Erroring clearly when neither or both is supplied.
fn resolve_sign_key(key: Option<&str>, key_env: Option<&str>) -> Result<SecretKey> {
    match (key, key_env) {
        (Some(path), None) => key_from_file(path),
        (None, Some(env)) => key_from_env(env),
        (Some(_), Some(_)) => bail!("pass exactly one of --key or --key-env, not both"),
        (None, None) => bail!("a signing key is required: pass --key <path> or --key-env <ENV>"),
    }
}

/// Sign an asset with `key`: return `(sha256, sig_b64, key_id)` over the §1
/// canonical message (which binds the optional `run`/`exec` launch overrides).
fn sign_artifact(a: &Artifact<'_>, key: &SecretKey) -> Result<(String, String, String)> {
    let id = key.public().key_id();
    let sha256 = sha256_hex_file(Path::new(a.path))?;
    let message = artifact_message(a, &sha256);
    let sig_b64 = B64.encode(key.sign(&message));
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
    let key = resolve_sign_key(key, key_env)?;
    let name = basename(artifact);
    let a = Artifact {
        name,
        version,
        path: artifact,
        run,
        exec,
    };
    let (sha256, sig_b64, id) = sign_artifact(&a, &key)?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "sha256: {sha256}")?;
    writeln!(stdout, "sig:    {sig_b64}")?;
    writeln!(stdout, "key_id: {id}")?;
    writeln!(stdout, "alg:    {}", key.algorithm())?;
    Ok(())
}

/// Reject malformed `--run`/`--exec` overrides up front, with the same rule the
/// loader applies at manifest parse — so a publisher cannot sign a value the
/// loader will refuse to load.
fn validate_overrides(run: Option<&str>, exec: Option<&str>) -> Result<()> {
    if let Some(run) = run {
        lode_core::manifest::validate_command_override("run", run)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    if let Some(exec) = exec {
        lode_core::manifest::validate_command_override("exec", exec)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(())
}

/// `verify` — recompute sha256 and check the signature locally. `run`/`exec` must
/// match the published overrides (they are part of the signed message). `public`
/// is the key as `keygen` prints it (base64, or `<alg>:<base64>` for ECDSA).
pub(crate) fn verify(
    artifact: &str,
    version: &str,
    run: Option<&str>,
    exec: Option<&str>,
    public: &str,
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
    let ok = verify_signature(public, &message, sig_b64)?;
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
    let key = key_from_file(key_path)?;
    let alg = key.algorithm();
    let (sha256, sig_b64, id) = sign_artifact(&a, &key)?;

    // The asset object: name/sha256/sig/key_id/run/exec are the signed identity +
    // digest + launch overrides; url/size are runtime fields (never signed); `alg`
    // is advisory (absent for ed25519). Format is derived from the filename at
    // install time, so it is not stored.
    let mut asset = Map::new();
    asset.insert("name".to_owned(), json!(name));
    asset.insert("url".to_owned(), json!(url));
    asset.insert("sha256".to_owned(), json!(sha256));
    asset.insert("sig".to_owned(), json!(sig_b64));
    asset.insert("key_id".to_owned(), json!(id));
    stamp_alg(&mut asset, alg);
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
        let mut man = json!({
            "schema": "lode/v1", "name": app, "key_id": id,
            "channels": { channel: { "latest": version } },
            "versions": { version: { "assets": [asset_obj] } },
        });
        if let Some(obj) = man.as_object_mut() {
            stamp_alg(obj, alg);
        }
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
        json!({ "schema": "lode/v1", "name": app, "channels": {}, "versions": {} })
    };
    let obj = root
        .as_object_mut()
        .context("manifest root is not a JSON object")?;
    obj.entry("schema").or_insert_with(|| json!("lode/v1"));
    obj.entry("name").or_insert_with(|| json!(app));
    // The top-level key_id (+ alg) is the default signer identity; an existing
    // one is preserved, as before.
    if !obj.contains_key("key_id") {
        obj.insert("key_id".to_owned(), json!(id));
        stamp_alg(obj, alg);
    }

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
/// `into`, computes the top-level signature over the canonical manifest
/// message (binding `name` + `key_id` + the channel/version catalog, EXCLUDING the
/// `sig` field), and writes the signer's `key_id` + `sig` (+ advisory `alg`) back
/// into the file.
///
/// The loader verifies this under `[trust].require_signature` (see
/// [`lode_core::install::verify_manifest_identity`]); both sides build the signed bytes
/// via [`lode_core::manifest::Manifest::signing_message`], so they always agree.
pub(crate) fn manifest_sign(into: &str, key_path: &str) -> Result<()> {
    let bytes = fs::read(into).with_context(|| format!("read {into}"))?;
    let mut manifest =
        lode_core::manifest::parse(&bytes).map_err(|e| anyhow::anyhow!("parse {into}: {e}"))?;

    let key = key_from_file(key_path)?;
    let id = key.public().key_id();

    // `key_id` is part of the signed message, so stamp it before building the bytes.
    manifest.key_id = Some(id.clone());
    let message = manifest.signing_message();
    let sig_b64 = B64.encode(key.sign(&message));

    // Write `key_id` + `sig` into the JSON, preserving everything else verbatim.
    let mut root: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {into} as JSON"))?;
    let obj = root
        .as_object_mut()
        .context("manifest root is not a JSON object")?;
    obj.insert("key_id".to_owned(), json!(id));
    obj.insert("sig".to_owned(), json!(sig_b64));
    stamp_alg(obj, key.algorithm());

    fs::write(into, format!("{}\n", serde_json::to_string_pretty(&root)?))
        .with_context(|| format!("write {into}"))?;
    writeln!(io::stdout().lock(), "signed {into}: key_id {id}")?;
    Ok(())
}

/// `init` — write the minimal starter `lode.toml` (the full documented reference
/// lives in `docs/lode.example.toml`). Shares the same template lode scaffolds on
/// first run ([`lode_core::config::STARTER_TOML`]).
pub(crate) fn init(path: Option<&str>) -> Result<()> {
    let template = lode_core::config::STARTER_TOML;
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

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lode-authoring-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `--key`/`--key-env` are mutually exclusive and one is required.
    #[test]
    fn resolve_sign_key_requires_exactly_one() {
        // Neither source → error.
        assert!(resolve_sign_key(None, None).is_err());
        // Both sources → error.
        assert!(resolve_sign_key(Some("/some.key"), Some("LODE_SIGNING_KEY")).is_err());
    }

    /// `--key` reads + decodes a base64 seed file (same path the env var feeds);
    /// an untagged seed is ed25519.
    #[test]
    fn resolve_sign_key_reads_file_seed() {
        let seed = [7u8; 32];
        let dir = scratch("seed");
        let key_path = dir.join("priv.key");
        fs::write(&key_path, B64.encode(seed)).unwrap();

        let got = resolve_sign_key(Some(key_path.to_str().unwrap()), None).unwrap();
        assert_eq!(got.algorithm(), Algorithm::Ed25519);
        assert_eq!(got.encode(), B64.encode(seed));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `--key-env` pointing at an unset variable errors clearly (it never reads a
    /// file). The set-and-decode path mirrors the file path through `SecretKey::parse`.
    #[test]
    fn key_env_unset_errors() {
        assert!(key_from_env("LODE_TEST_UNSET_SIGNING_KEY_VAR_XYZ").is_err());
        assert!(resolve_sign_key(None, Some("LODE_TEST_UNSET_SIGNING_KEY_VAR_XYZ")).is_err());
    }

    /// `keygen --out` writes the PRIVATE key owner-only (0600) — both on first
    /// creation and when re-run over an existing world-readable key file.
    #[cfg(unix)]
    #[test]
    fn keygen_private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("keygen");
        let prefix = dir.join("id");
        let prefix = prefix.to_str().unwrap();
        let key_path = dir.join("id.key");

        keygen(Some(prefix), "ed25519").unwrap();
        let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Re-running over a pre-existing 0644 key tightens it back to 0600.
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
        keygen(Some(prefix), "ed25519").unwrap();
        let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = fs::remove_dir_all(&dir);
    }

    /// ed25519 keygen output keeps the pre-`alg` file forms: an untagged base64
    /// seed and a `<key_id> <base64>` public line.
    #[test]
    fn keygen_ed25519_files_are_untagged() {
        let dir = scratch("ed-files");
        let prefix = dir.join("id");
        keygen(Some(prefix.to_str().unwrap()), "ed25519").unwrap();
        let key = fs::read_to_string(dir.join("id.key")).unwrap();
        let public = fs::read_to_string(dir.join("id.pub")).unwrap();
        assert!(!key.contains(':'));
        assert_eq!(public.split_whitespace().count(), 2);
        assert!(!public.contains(':'));
        let _ = fs::remove_dir_all(&dir);
    }

    /// An ECDSA keygen writes tagged files, the private key round-trips through
    /// `--key`, and the `.pub` line works verbatim as a trusted-key entry for a
    /// signature the key produced.
    #[test]
    fn keygen_ecdsa_files_are_tagged_and_roundtrip() {
        for alg in [Algorithm::EcdsaP256, Algorithm::EcdsaP384] {
            let dir = scratch(alg.name());
            let prefix = dir.join("id");
            keygen(Some(prefix.to_str().unwrap()), alg.name()).unwrap();

            let key_path = dir.join("id.key");
            let key_text = fs::read_to_string(&key_path).unwrap();
            assert!(key_text.starts_with(&format!("{alg}:")), "{key_text}");
            let public_line = fs::read_to_string(dir.join("id.pub")).unwrap();
            assert!(public_line.starts_with(&format!("{alg}:")), "{public_line}");

            let key = resolve_sign_key(Some(key_path.to_str().unwrap()), None).unwrap();
            assert_eq!(key.algorithm(), alg);
            // The env-var form is the file's content.
            assert_eq!(SecretKey::parse(&key_text).unwrap().encode(), key.encode());

            let artifact_path = dir.join("app.bin");
            fs::write(&artifact_path, b"payload").unwrap();
            let a = Artifact {
                name: "app.bin",
                version: "1.0.0",
                path: artifact_path.to_str().unwrap(),
                run: None,
                exec: None,
            };
            let (sha256, sig_b64, id) = sign_artifact(&a, &key).unwrap();
            assert!(public_line.contains(&id));
            let trusted = vec![public_line.trim().to_owned()];
            assert!(
                lode_core::verify::verify_artifact_sig(
                    "app.bin", "1.0.0", &sha256, None, None, &sig_b64, &trusted
                )
                .is_ok()
            );
            // An ed25519-labelled (untagged) copy of the same public key is not
            // usable: the algorithm travels with the key.
            let untagged = public_line.trim().split_once(':').unwrap().1.to_owned();
            assert!(
                lode_core::verify::verify_artifact_sig(
                    "app.bin",
                    "1.0.0",
                    &sha256,
                    None,
                    None,
                    &sig_b64,
                    &[untagged]
                )
                .is_err()
            );
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn keygen_rejects_unknown_algorithm() {
        let err = keygen(None, "hmac-sha256").unwrap_err().to_string();
        assert!(
            err.contains("hmac-sha256") && err.contains("ecdsa-p256"),
            "{err}"
        );
    }

    /// `manifest-sign` stamps `alg` only for non-ed25519 keys, and clears a stale
    /// one when re-signed with ed25519.
    #[test]
    fn manifest_sign_stamps_and_clears_alg() {
        let dir = scratch("manifest-sign");
        let manifest_path = dir.join("manifest.json");
        fs::write(
            &manifest_path,
            r#"{"schema":"lode/v1","name":"x",
                "channels":{"stable":{"latest":"1.0.0"}},
                "versions":{"1.0.0":{"assets":[{"name":"a","url":"","sha256":"00"}]}}}"#,
        )
        .unwrap();
        let ecdsa = dir.join("ecdsa");
        let ed = dir.join("ed");
        keygen(Some(ecdsa.to_str().unwrap()), "ecdsa-p256").unwrap();
        keygen(Some(ed.to_str().unwrap()), "ed25519").unwrap();
        let read = || -> Value {
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap()
        };

        manifest_sign(
            manifest_path.to_str().unwrap(),
            dir.join("ecdsa.key").to_str().unwrap(),
        )
        .unwrap();
        let signed = read();
        assert_eq!(signed["alg"], json!("ecdsa-p256"));
        assert!(signed["sig"].is_string());
        // The stamped signature verifies against the `.pub` line.
        let parsed = lode_core::manifest::parse(&fs::read(&manifest_path).unwrap()).unwrap();
        let trusted = vec![
            fs::read_to_string(dir.join("ecdsa.pub"))
                .unwrap()
                .trim()
                .to_owned(),
        ];
        assert!(
            lode_core::verify::verify_manifest_sig(
                &trusted,
                parsed.key_id.as_deref(),
                &parsed.signing_message(),
                parsed.sig.as_deref().unwrap()
            )
            .is_ok()
        );

        manifest_sign(
            manifest_path.to_str().unwrap(),
            dir.join("ed.key").to_str().unwrap(),
        )
        .unwrap();
        assert!(read().get("alg").is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
