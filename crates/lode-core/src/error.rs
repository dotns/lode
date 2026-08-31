//! Central error type for lode.
//!
//! One [`Error`] threads through every module via the crate [`Result`] alias.
//! `#[from]` conversions let leaf modules use `?` directly on foreign errors,
//! while the message-carrying domain variants give each subsystem a typed home.
//! The message-carrying domain variants are deliberately broad; `#[allow(dead_code)]`
//! covers the few not constructed on every build path.

use thiserror::Error;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Every fallible operation in lode surfaces as one of these.
#[derive(Debug, Error)]
#[allow(dead_code)] // not every domain variant is constructed on all build paths
pub enum Error {
    /// Configuration could not be resolved or failed validation.
    #[error("config: {0}")]
    Config(String),

    /// HTTP fetch of a manifest, artifact or runtime failed.
    #[error("http: {0}")]
    Http(String),

    /// A remote manifest was malformed or referenced a missing entry.
    #[error("manifest: {0}")]
    Manifest(String),

    /// Downloading or unpacking an artifact failed.
    #[error("download: {0}")]
    Download(String),

    /// Installing a version (atomic swap, permissions, GC) failed.
    #[error("install: {0}")]
    Install(String),

    /// Integrity (sha256) or signature (ed25519) verification failed.
    #[error("verify: {0}")]
    Verify(String),

    /// The PID lock could not be acquired or a stale lock could not be reclaimed.
    #[error("lock: {0}")]
    Lock(String),

    /// Reading or writing `state.json` failed beyond a plain I/O error.
    #[error("state: {0}")]
    State(String),

    /// Spawning, signalling or supervising the child process failed.
    #[error("process: {0}")]
    Process(String),

    /// Filesystem / I/O error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (manifest / state) (de)serialisation error.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML (`lode.toml`) parse error — sanitized message only (see the manual
    /// `From<toml::de::Error>` below).
    #[error("toml: {0}")]
    Toml(String),

    /// Integer parse error (numeric config supplied as text).
    #[error("parse: {0}")]
    ParseInt(#[from] std::num::ParseIntError),
}

/// Sanitizing conversion: `toml::de::Error`'s `Display` echoes the offending
/// source line (which in a malformed `lode.toml` could be a literal secret), so
/// keep only the parser message plus the byte span — never the snippet. The
/// message still names unknown/invalid *keys*, which is the useful part.
impl From<toml::de::Error> for Error {
    fn from(e: toml::de::Error) -> Self {
        let msg = e.span().map_or_else(
            || e.message().to_owned(),
            |span| format!("{} (at bytes {}..{})", e.message(), span.start, span.end),
        );
        Self::Toml(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_error_never_echoes_file_content() {
        // A malformed line carrying a literal secret: the unquoted value is a
        // parse error, and the sanitized variant must not leak it.
        let bad = "[http]\ntoken = sk-SECRET-VALUE\n";
        let err: Error = toml::from_str::<toml::Value>(bad).unwrap_err().into();
        let rendered = err.to_string();
        assert!(rendered.starts_with("toml: "), "got: {rendered}");
        assert!(!rendered.contains("SECRET"), "leaked content: {rendered}");
    }
}
