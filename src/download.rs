//! Streaming artifact download: body → `$DATA_DIR/downloads/<ver>.part`, with the
//! sha256 taken over the downloaded file (pre-unpack, design §4/§6).
//!
//! The bytes are streamed to disk (bounded memory) and then digested via the
//! shared [`crate::verify::sha256_hex_file`] — reusing the audited hashing path
//! rather than re-implementing it. The caller verifies the digest + signature and
//! unpacks (see [`crate::install`]); on failure the `.part` file is left for the
//! startup GC ([`crate::install::gc`]) to reap.

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

/// Download `asset` (for `version`) to `$DATA_DIR/downloads/<version>.part`,
/// returning the temp path and the lowercase-hex sha256 of the downloaded file.
///
/// `[http].headers` (often a bearer token) are attached only when the asset opts in
/// (`auth`, the default) **and** its host is in `allowed_hosts` — the source
/// same-origin set plus `[http].credential_hosts` (computed by the caller, see
/// [`crate::manifest::allowed_hosts`]). A manifest that points an asset at any other
/// host gets no credentials (they are dropped, with a warning), so a hostile
/// manifest can't redirect the token to an attacker. An optional `size` is enforced
/// (the temp file is removed on a mismatch). Integrity (`sha256 == asset.sha256`)
/// is the caller's check.
pub(crate) fn fetch_artifact(
    cfg: &Config,
    asset: &Asset,
    version: &str,
    allowed_hosts: &[String],
) -> Result<(PathBuf, String)> {
    // `version` keys `downloads/<version>.part`; reject traversal before the join
    // (the runtime path passes "runtime", a valid id).
    validate_id("version", version)?;
    let downloads = cfg.global.data_dir.join("downloads");
    fs::create_dir_all(&downloads)?;
    let temp = downloads.join(format!("{version}.part"));

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

    let reader = crate::http::get_reader(&asset.url, &headers, cfg.http.allow_insecure)?;
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
    Ok((temp, sha256))
}

/// Whether credentials may ride a download to `url`: true only when the URL's
/// host is in `allowed_hosts` (case-insensitively). Split out from
/// [`fetch_artifact`] so the same-origin gate is unit-testable without a network,
/// and reused by [`crate::manifest`]'s native `.sig` sidecar fetch (§6) so the
/// sidecar obeys the identical same-origin credential rule.
pub(crate) fn host_allowed(url: &str, allowed_hosts: &[String]) -> bool {
    crate::http::url_host(url)
        .is_some_and(|host| allowed_hosts.iter().any(|a| a.eq_ignore_ascii_case(host)))
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
