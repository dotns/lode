//! Streaming artifact download with a persistent per-version cache: the body is
//! streamed to `$DATA_DIR/downloads/<ver>/<asset>.part` (bounded memory) and, once
//! fully written, promoted to `$DATA_DIR/downloads/<ver>/<asset>`. The sha256 is
//! taken over the downloaded file (pre-unpack, design §4/§6) via the shared
//! [`crate::verify::sha256_hex_file`] — reusing the audited hashing path rather than
//! re-implementing it.
//!
//! Download is decoupled from launch. A verified artifact is *kept* after extraction,
//! so a fetch happens only when the cache is absent or fails its integrity check (see
//! [`reusable_cache`]) and a later launch can re-extract it without re-downloading.
//! The caller verifies the digest + signature and unpacks (see [`crate::install`]);
//! the cached artifact is reclaimed only by version pruning
//! ([`crate::install::prune`]), while an interrupted `.part` is left for the startup
//! GC ([`crate::install::gc`]) to reap.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::idval::validate_id;
use crate::manifest::Asset;

/// Hard ceiling on a streamed artifact body, enforced even when `artifact.size`
/// is absent. Generous (most runtimes/apps are far smaller) but bounded so an
/// endless or oversized response can't fill the disk (design §16, `DoS` guard).
const MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Ensure `asset` (for `version`) is present and verified at
/// `$DATA_DIR/downloads/<version>/<asset>`, returning that path and the lowercase-hex
/// sha256 of the file.
///
/// A previously-downloaded artifact whose digest still matches `asset.sha256` is
/// reused without touching the network ([`reusable_cache`]) — only an absent or
/// corrupt cache triggers a fetch, so download is decoupled from launch.
///
/// On a fetch, `[http].headers` (often a bearer token) are attached only when the
/// asset opts in (`auth`, the default) **and** its host is in `allowed_hosts` — the
/// source same-origin set plus `[http].credential_hosts` (computed by the caller, see
/// [`crate::manifest::allowed_hosts`]). A manifest that points an asset at any other
/// host gets no credentials (they are dropped, with a warning), so a hostile manifest
/// can't redirect the token to an attacker. An optional `size` is enforced (the
/// `.part` is removed on a mismatch). The body is streamed to a `.part` sibling and,
/// once fully written, atomically renamed into place. The integrity
/// (`sha256 == asset.sha256`) + signature gate stays the caller's check
/// ([`crate::install`]).
pub(crate) fn fetch_artifact(
    cfg: &Config,
    asset: &Asset,
    version: &str,
    allowed_hosts: &[String],
) -> Result<(PathBuf, String)> {
    // `version` keys `downloads/<version>/` and `asset.name` the file within it;
    // reject traversal in either before they reach a path join (the runtime path
    // passes "runtime"/the runtime name, both valid ids).
    validate_id("version", version)?;
    validate_id("asset", &asset.name)?;
    let cache_dir = cfg.global.data_dir.join("downloads").join(version);
    let cache_path = cache_dir.join(&asset.name);

    if let Some(hit) = reusable_cache(&cache_path, &asset.sha256) {
        tracing::info!(
            version,
            artifact = %cache_path.display(),
            "reusing cached download; skipping fetch"
        );
        return Ok(hit);
    }

    fs::create_dir_all(&cache_dir)?;
    let temp = cache_dir.join(format!("{}.part", asset.name));

    let headers = if asset.auth && !cfg.http.headers.is_empty() {
        if host_allowed(&asset.url, allowed_hosts) {
            crate::http::expand_headers(&cfg.http.headers)?
        } else {
            // Cross-origin (relative to the source) and not allowlisted: drop the
            // credentials rather than leak them to the asset host. Only the host
            // (never a header value) is logged.
            tracing::warn!(
                host = crate::http::url_host(&asset.url).unwrap_or("?"),
                "credentials not attached to cross-origin host"
            );
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // The same gate re-applies on every redirect hop: a host that 302s the
    // download outside the allowlist strips the credentials for that hop (see
    // [`crate::http::send`]'s redirect policy).
    let attach = |host: &str| host_in(host, allowed_hosts);
    let reader = crate::http::get_reader(&asset.url, &headers, cfg.http.allow_insecure, &attach)?;
    let written = write_stream(reader, &temp, MAX_DOWNLOAD_BYTES)?;

    if let Some(expected) = asset.size
        && written != expected
    {
        let _ = fs::remove_file(&temp);
        return Err(Error::Download(format!(
            "size mismatch for {version}: expected {expected} bytes, got {written}"
        )));
    }

    let sha256 = crate::verify::sha256_hex_file(&temp)
        .map_err(|e| Error::Download(format!("hash {}: {e}", temp.display())))?;
    // Promote the fully-written, size-checked `.part` into the cache under its real
    // name. The retained file lets a later launch re-extract without re-downloading;
    // the digest/signature gate stays the caller's job ([`crate::install`]).
    fs::rename(&temp, &cache_path)
        .map_err(|e| Error::Download(format!("cache {}: {e}", cache_path.display())))?;
    Ok((cache_path, sha256))
}

/// Whether a cached download at `cache_path` may be reused instead of re-fetching,
/// returning its `(path, sha256)` when so. Reuse requires the file to be present and
/// to hash to `expected_sha` (lowercase hex) — so a swapped or truncated cache is
/// never trusted. An empty `expected_sha` (the runtime download carries no manifest
/// digest) is treated as non-reusable, so such a download is always re-fetched fresh
/// rather than risk serving stale bytes. A present-but-corrupt or unreadable cache
/// file is removed and reported as a miss, so the next call re-downloads cleanly.
/// Split out from [`fetch_artifact`] so the decision is unit-testable without a
/// network.
fn reusable_cache(cache_path: &Path, expected_sha: &str) -> Option<(PathBuf, String)> {
    if expected_sha.trim().is_empty() || !cache_path.is_file() {
        return None;
    }
    match crate::verify::sha256_hex_file(cache_path) {
        Ok(sha) if sha.eq_ignore_ascii_case(expected_sha.trim()) => {
            Some((cache_path.to_path_buf(), sha))
        }
        _ => {
            let _ = fs::remove_file(cache_path);
            None
        }
    }
}

/// Whether credentials may ride a download to `url`: true only when the URL's
/// host is in `allowed_hosts` (case-insensitively). Split out from
/// [`fetch_artifact`] so the same-origin gate is unit-testable without a network,
/// and reused by [`crate::manifest`]'s native `.sig` sidecar fetch (§6) so the
/// sidecar obeys the identical same-origin credential rule.
pub(crate) fn host_allowed(url: &str, allowed_hosts: &[String]) -> bool {
    crate::http::url_host(url).is_some_and(|host| host_in(host, allowed_hosts))
}

/// The host-level form of [`host_allowed`], reused as the per-redirect-hop
/// attach predicate (see [`crate::http::send`]'s redirect policy): each hop's
/// host must itself be allowlisted or the custom headers are dropped for that
/// hop, so a redirecting server can't bounce the credentials to a third host.
pub(crate) fn host_in(host: &str, allowed_hosts: &[String]) -> bool {
    allowed_hosts.iter().any(|a| a.eq_ignore_ascii_case(host))
}

/// Stream `reader` to `dest`, returning the number of bytes written, refusing a
/// body larger than `max` bytes. Split out from [`fetch_artifact`] so the disk
/// path (and the cap) is unit-testable without a network; `max` is a parameter so
/// a test can exercise the bound with a small value.
///
/// The reader is `take`-capped at `max + 1`, so at most one byte past the limit
/// ever reaches disk before the oversized body is rejected and the partial
/// `.part` file removed — an endless body can't run the disk out of space.
fn write_stream(reader: impl Read, dest: &Path, max: u64) -> Result<u64> {
    let mut file = fs::File::create(dest)
        .map_err(|e| Error::Download(format!("create {}: {e}", dest.display())))?;
    let mut limited = reader.take(max.saturating_add(1));
    let written = io::copy(&mut limited, &mut file)
        .map_err(|e| Error::Download(format!("write {}: {e}", dest.display())))?;
    if written > max {
        drop(file);
        let _ = fs::remove_file(dest);
        return Err(Error::Download(format!(
            "download body exceeds {max} byte cap"
        )));
    }
    file.sync_all()?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lode-download-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_stream_persists_bytes_and_hash_matches() {
        let dir = scratch("ws");
        let dest = dir.join("x.part");
        let data = b"hello lode \x00\x01\x02 stream";

        let written = write_stream(&data[..], &dest, 1024).unwrap();
        assert_eq!(written, data.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), data);

        // The on-disk digest (download path) equals the in-memory digest.
        assert_eq!(
            crate::verify::sha256_hex_file(&dest).unwrap(),
            crate::verify::sha256_hex(data)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_stream_rejects_body_past_cap() {
        let dir = scratch("cap");
        let dest = dir.join("big.part");
        // A body one byte over the cap is rejected and the partial file removed,
        // so an endless/oversized response can't fill the disk.
        let body = [0u8; 65];
        let err = write_stream(&body[..], &dest, 64).unwrap_err();
        assert!(matches!(err, Error::Download(_)));
        assert!(
            !dest.exists(),
            "the oversized .part is removed on rejection"
        );

        // A body exactly at the cap still succeeds.
        let ok = [7u8; 64];
        assert_eq!(write_stream(&ok[..], &dest, 64).unwrap(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reusable_cache_reuses_only_a_digest_matched_file() {
        let dir = scratch("cache");
        let bytes = b"cached image bytes";
        let sha = crate::verify::sha256_hex(bytes);
        let path = dir.join("app.tar.gz");

        // Absent → miss (the caller must fetch).
        assert!(reusable_cache(&path, &sha).is_none());

        // Present and the digest matches → reuse without a network fetch.
        std::fs::write(&path, bytes).unwrap();
        let hit = reusable_cache(&path, &sha).expect("a digest-matched cache is reused");
        assert_eq!(hit.0, path);
        assert_eq!(hit.1, sha);

        // Present but the digest mismatches → the corrupt cache is dropped and the
        // call reports a miss, so the next fetch re-downloads cleanly.
        assert!(reusable_cache(&path, &"00".repeat(32)).is_none());
        assert!(!path.exists(), "a corrupt cache file is removed");

        // An empty expected digest (e.g. the runtime download) never reuses, so a
        // stale runtime archive can't be served — and the file is left in place.
        std::fs::write(&path, bytes).unwrap();
        assert!(reusable_cache(&path, "").is_none());
        assert!(path.exists(), "a non-verifiable cache is left, not deleted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_allowed_gates_on_same_origin_and_allowlist() {
        let source = ["releases.example".to_owned()];
        // Same-origin as the source → credentials attach.
        assert!(host_allowed(
            "https://releases.example/myapp/app.tar.gz",
            &source
        ));
        // A foreign host → credentials dropped.
        assert!(!host_allowed("https://evil.example/app.tar.gz", &source));
        // An explicit allowlist entry (e.g. a separate CDN) → attach.
        let with_cdn = ["releases.example".to_owned(), "cdn.example".to_owned()];
        assert!(host_allowed("https://cdn.example/app.tar.gz", &with_cdn));
        // Host comparison is case-insensitive; the port is ignored.
        assert!(host_allowed("https://Releases.Example:443/app", &source));
        // An empty allowlist (no source, no operator hosts) attaches to nothing.
        assert!(!host_allowed("https://releases.example/app", &[]));
        // A non-http(s) / hostless URL is never allowed.
        assert!(!host_allowed("ftp://releases.example/app", &source));
    }
}
