//! Integrity (sha256) + publisher identity (ed25519) verification.
//!
//! These are the runtime primitives the loader uses to verify a downloaded
//! artifact. The publisher-side `keygen` / `sign` / `verify` / `manifest`
//! commands (exposed under `lode-cli`) reuse them — see [`crate::authoring`].
//!
//! Keys are raw 32-byte ed25519 values, distributed as base64. A `key_id` is the
//! first 16 hex chars of `sha256(public_key)`. The signed message binds an
//! artifact's identity to its content digest; see [`artifact_message`].

use std::fs::File;
use std::io;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use sha2::{Digest as _, Sha256};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Identity of a single release asset, used to build the signed message. `name`
/// is the **asset filename** — the only field binding *which* artifact a signature
/// authorises (§1). `format`/`entry`/`url` are derived from the filename or are
/// operator-local, so they are deliberately not part of this struct.
pub(crate) struct Artifact<'a> {
    /// The asset filename (selection key + signed identity).
    pub(crate) name: &'a str,
    pub(crate) version: &'a str,
    /// On-disk path to hash (authoring only); not part of the signed message.
    pub(crate) path: &'a str,
}

/// Lowercase hex encoding.
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// `key_id` = first 16 hex chars of `sha256(public_key)`.
pub(crate) fn key_id(public: &[u8; 32]) -> String {
    let digest = Sha256::digest(public);
    to_hex(&digest)[..16].to_owned()
}

/// Canonical ed25519 message binding an asset's identity (`name` = the asset
/// filename, plus `version`) to its content digest. Exact bytes (UTF-8, `\n`
/// separated, no trailing newline) — must match the loader. `format`/`entry`/`url`
/// are *not* bound: the filename's extension fixes the format and `entry`/`url` are
/// runtime concerns (§1/§3/§4).
pub(crate) fn artifact_message(a: &Artifact<'_>, sha256_hex: &str) -> Vec<u8> {
    format!(
        "lode.artifact.v1\n{}\n{}\n{}",
        a.name, a.version, sha256_hex
    )
    .into_bytes()
}

/// Stream a file through sha256 and return the lowercase hex digest.
pub(crate) fn sha256_hex_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).with_context(|| format!("read {}", path.display()))?;
    Ok(to_hex(&hasher.finalize()))
}

/// Verify a base64 signature over an artifact message against a base64 public key.
pub(crate) fn verify_signature(public_b64: &str, message: &[u8], sig_b64: &str) -> Result<bool> {
    let public = decode_key(public_b64).context("decode public key")?;
    let verifying = VerifyingKey::from_bytes(&public).context("invalid ed25519 public key")?;
    let sig_bytes = B64
        .decode(sig_b64.trim())
        .context("decode signature base64")?;
    let signature = Signature::from_slice(&sig_bytes).context("invalid ed25519 signature")?;
    Ok(verifying.verify(message, &signature).is_ok())
}

/// Lowercase-hex sha256 of an in-memory buffer. Companion to
/// [`sha256_hex_file`] for callers that already hold the bytes (the
/// runtime-download path and unit tests). Reuses the same `to_hex` encoding.
#[allow(dead_code)] // in-memory digest helper; used by the download path + tests
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    to_hex(&Sha256::digest(bytes))
}

/// Verify an asset's ed25519 signature against a set of trusted keys, using the
/// exact §1 canonical message (`lode.artifact.v1\n{name}\n{version}\n{sha256}`,
/// where `name` is the asset filename). Each entry in `trusted_keys` is
/// `key_id:base64` (CLI/TOML form) or `key_id base64` (file form); the base64
/// public component is extracted from either. Succeeds as soon as any trusted
/// key validates the signature; errors if none do. The integrity (sha256) check
/// is the caller's responsibility (see [`crate::install`]).
pub(crate) fn verify_artifact_sig(
    name: &str,
    version: &str,
    sha256_hex: &str,
    sig_b64: &str,
    trusted_keys: &[String],
) -> Result<()> {
    if trusted_keys.is_empty() {
        bail!("no trusted keys configured to verify the artifact signature");
    }
    let artifact = Artifact {
        name,
        version,
        path: "",
    };
    let message = artifact_message(&artifact, sha256_hex);
    for key in trusted_keys {
        // A malformed key (bad base64 / wrong length) is skipped, not fatal —
        // another configured key (e.g. during rotation) may still validate.
        if matches!(
            verify_signature(trusted_key_public(key), &message, sig_b64),
            Ok(true)
        ) {
            return Ok(());
        }
    }
    bail!("artifact signature did not match any trusted key");
}

/// Canonical ed25519 message binding a manifest's identity (`name` +
/// `key_id`) to a deterministic, `sig`-free serialization of its catalog
/// (`canonical` — built by [`crate::manifest::Manifest::signing_message`] from the
/// sorted channel/version maps). Exact bytes (UTF-8, `\n`-separated, no trailing
/// newline beyond what `canonical` carries): `lode.manifest.v1\n{name}\n{key_id}\n{canonical}`.
/// The publisher (`lode-cli manifest-sign`) and the loader MUST produce identical
/// bytes; both go through `signing_message` so they always do.
pub(crate) fn manifest_message(name: &str, key_id: &str, canonical: &str) -> Vec<u8> {
    format!("lode.manifest.v1\n{name}\n{key_id}\n{canonical}").into_bytes()
}

/// Verify a manifest's top-level ed25519 signature over its canonical message
/// against a set of trusted keys. The manifest's declared `key_id` selects the
/// preferred key; when it is `None` or matches no trusted entry, every trusted key
/// is tried (covering an absent id or a rotation where the id differs). Succeeds as
/// soon as any key validates the signature; errors if none do. Entry forms are the
/// same `key_id:base64` / `key_id base64` / bare `base64` accepted elsewhere.
pub(crate) fn verify_manifest_sig(
    trusted_keys: &[String],
    key_id: Option<&str>,
    message: &[u8],
    sig_b64: &str,
) -> Result<()> {
    if trusted_keys.is_empty() {
        bail!("no trusted keys configured to verify the manifest signature");
    }
    // Prefer the key whose id matches the manifest's declared `key_id`.
    if let Some(want) = key_id
        && let Some(entry) = trusted_keys
            .iter()
            .find(|e| trusted_key_id(e).as_deref() == Some(want))
        && matches!(
            verify_signature(trusted_key_public(entry), message, sig_b64),
            Ok(true)
        )
    {
        return Ok(());
    }
    // Fall back to trying every trusted key (a missing/unmatched id, or rotation).
    for entry in trusted_keys {
        if matches!(
            verify_signature(trusted_key_public(entry), message, sig_b64),
            Ok(true)
        ) {
            return Ok(());
        }
    }
    bail!("manifest signature did not match any trusted key");
}

/// The `key_id` of a trusted-key entry, derived from its base64 public component,
/// or `None` when the entry is malformed (bad base64 / wrong length).
fn trusted_key_id(entry: &str) -> Option<String> {
    decode_key(trusted_key_public(entry))
        .ok()
        .map(|public| key_id(&public))
}

/// Extract the base64 public-key component from a trusted-key entry, accepting
/// `key_id:base64`, `key_id base64`, or a bare `base64`. (The base64 alphabet
/// contains no `:` or whitespace, so the split is unambiguous.)
fn trusted_key_public(entry: &str) -> &str {
    let entry = entry.trim();
    entry
        .split_once(':')
        .or_else(|| entry.split_once(char::is_whitespace))
        .map_or(entry, |(_, key)| key.trim())
}

/// Decode a base64 32-byte key.
pub(crate) fn decode_key(b64: &str) -> Result<[u8; 32]> {
    let bytes = B64.decode(b64.trim()).context("base64 decode")?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 32-byte key, got {} bytes", bytes.len()))?;
    Ok(arr)
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    #[test]
    fn sign_then_verify_roundtrip() {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).unwrap();
        let signing = SigningKey::from_bytes(&seed);
        let public_b64 = B64.encode(signing.verifying_key().to_bytes());

        let artifact = Artifact {
            name: "app-linux-x86_64.tar.gz",
            version: "1.0.0",
            path: "",
        };
        let msg = artifact_message(&artifact, "abc123");
        let sig_b64 = B64.encode(signing.sign(&msg).to_bytes());

        assert!(verify_signature(&public_b64, &msg, &sig_b64).unwrap());

        // Tampered message must fail.
        let bad = artifact_message(&artifact, "deadbeef");
        assert!(!verify_signature(&public_b64, &bad, &sig_b64).unwrap());
    }

    #[test]
    fn key_id_is_stable_16_hex() {
        let id = key_id(&[7u8; 32]);
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // The empty-input sha256 is a well-known vector.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn verify_artifact_sig_happy_and_tampered() {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).unwrap();
        let signing = SigningKey::from_bytes(&seed);
        let public = signing.verifying_key().to_bytes();
        let id = key_id(&public);
        let public_b64 = B64.encode(public);

        let sha = "a97ad2265ae84cdeff1219b1c83db8e6f096e444c81f733bc93355f0fff368a1";
        let name = "myapp-linux-x86_64.tar.gz";
        let artifact = Artifact {
            name,
            version: "1.5.0",
            path: "",
        };
        let sig = B64.encode(signing.sign(&artifact_message(&artifact, sha)).to_bytes());

        // Fixed identity (name + version); vary only the digest + key set.
        let check =
            |sha: &str, keys: &[String]| verify_artifact_sig(name, "1.5.0", sha, &sig, keys);

        // CLI/TOML form `key_id:base64` and bare base64 both validate.
        let keys_colon = vec![format!("{id}:{public_b64}")];
        let keys_bare = vec![public_b64.clone()];
        let keys_space = vec![format!("{id} {public_b64}")]; // file form
        for keys in [&keys_colon, &keys_bare, &keys_space] {
            assert!(check(sha, keys).is_ok());
        }

        // The digest is part of the message: changing it must fail.
        let bad_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef0";
        assert!(check(bad_sha, &keys_colon).is_err());

        // A signature is bound to the asset filename: the same bytes under a
        // different asset name must fail.
        assert!(
            verify_artifact_sig("other-asset.tar.gz", "1.5.0", sha, &sig, &keys_colon).is_err()
        );
        // …and under a different version.
        assert!(verify_artifact_sig(name, "9.9.9", sha, &sig, &keys_colon).is_err());

        // No trusted keys → error (cannot establish identity).
        assert!(check(sha, &[]).is_err());

        // An untrusted key alone → error.
        let other = B64.encode(
            SigningKey::from_bytes(&[9u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        assert!(check(sha, &[other]).is_err());
    }

    #[test]
    fn trusted_key_public_parses_all_forms() {
        assert_eq!(trusted_key_public("abc123:KEYDATA"), "KEYDATA");
        assert_eq!(trusted_key_public("abc123 KEYDATA"), "KEYDATA");
        assert_eq!(trusted_key_public("  KEYDATA  "), "KEYDATA");
    }

    #[test]
    fn manifest_message_is_stable_and_excludes_sig() {
        // The bytes are a fixed, reproducible function of (name, key_id, canonical).
        let msg = manifest_message("myapp", "deadbeefdeadbeef", "channel\tstable\t1.0.0\n");
        assert_eq!(
            msg,
            b"lode.manifest.v1\nmyapp\ndeadbeefdeadbeef\nchannel\tstable\t1.0.0\n".to_vec()
        );
        // A different catalog produces different bytes (so a tampered catalog fails).
        let other = manifest_message("myapp", "deadbeefdeadbeef", "channel\tstable\t9.9.9\n");
        assert_ne!(msg, other);
    }

    #[test]
    fn verify_manifest_sig_happy_tampered_and_wrong_key() {
        let signing = SigningKey::from_bytes(&[11u8; 32]);
        let public = signing.verifying_key().to_bytes();
        let id = key_id(&public);
        let public_b64 = B64.encode(public);

        let canonical = "channel\tstable\t1.0.0\nversion\t1.0.0\nasset\tapp-linux.tar.gz\tabc\n";
        let message = manifest_message("myapp", &id, canonical);
        let sig = B64.encode(signing.sign(&message).to_bytes());

        let trusted = vec![format!("{id}:{public_b64}")];
        // Happy path: id selects the matching key and it validates.
        assert!(verify_manifest_sig(&trusted, Some(&id), &message, &sig).is_ok());
        // key_id None still succeeds via the try-all fallback.
        assert!(verify_manifest_sig(&trusted, None, &message, &sig).is_ok());

        // Tampered message (catalog changed) → fail.
        let tampered = manifest_message("myapp", &id, "channel\tstable\t2.0.0\n");
        assert!(verify_manifest_sig(&trusted, Some(&id), &tampered, &sig).is_err());

        // Wrong key (an untrusted signer) → fail, even though its id is advertised.
        let attacker = SigningKey::from_bytes(&[12u8; 32]);
        let attacker_pub = attacker.verifying_key().to_bytes();
        let attacker_id = key_id(&attacker_pub);
        let attacker_msg = manifest_message("myapp", &attacker_id, canonical);
        let attacker_sig = B64.encode(attacker.sign(&attacker_msg).to_bytes());
        assert!(
            verify_manifest_sig(&trusted, Some(&attacker_id), &attacker_msg, &attacker_sig)
                .is_err()
        );

        // No trusted keys → error (cannot establish identity).
        assert!(verify_manifest_sig(&[], Some(&id), &message, &sig).is_err());
    }

    #[test]
    fn trusted_key_id_derives_from_public() {
        let public = SigningKey::from_bytes(&[7u8; 32])
            .verifying_key()
            .to_bytes();
        let id = key_id(&public);
        let public_b64 = B64.encode(public);
        // All three entry forms resolve to the same derived id.
        assert_eq!(
            trusted_key_id(&format!("ignored:{public_b64}")).as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            trusted_key_id(&format!("ignored {public_b64}")).as_deref(),
            Some(id.as_str())
        );
        assert_eq!(trusted_key_id(&public_b64).as_deref(), Some(id.as_str()));
        // A malformed entry yields None rather than erroring.
        assert!(trusted_key_id("not-base64-!!!").is_none());
    }
}
