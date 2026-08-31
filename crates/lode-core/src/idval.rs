//! Path-component validation for untrusted ids (design §4/§15, security P0).
//!
//! A manifest `versions` key, a GitHub release tag, and a `[runtime].runtime`
//! name all flow from the network into filesystem paths (`versions/<ver>`,
//! `downloads/<ver>/<asset>`, `runtime/<name>`). Left unchecked, an id like
//! `../../etc` would escape the data dir.
//!
//! This module is the single validation layer the loader enforces *before* any
//! such id reaches a path join: [`validate_id`] is strict — a version or runtime
//! name must be one safe path component. It is hand-rolled (no new dependency)
//! and never echoes a control character raw (the offending id is rendered with
//! `{:?}`).

use crate::error::{Error, Result};

/// Validate a `version` id or runtime `name`: it must be exactly one safe path
/// component. Rejects an empty id, any path separator (`/` or `\`), a `.`/`..`
/// component, a leading `.` or `-`, any control character, and anything outside
/// `[A-Za-z0-9._-]`. `kind` names the field for the error message.
pub(crate) fn validate_id(kind: &str, id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(invalid(kind, id, "must not be empty"));
    }
    for c in id.chars() {
        if c.is_control() {
            return Err(invalid(kind, id, "contains a control character"));
        }
        if c == '/' || c == '\\' {
            return Err(invalid(kind, id, "contains a path separator"));
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(invalid(kind, id, "contains a disallowed character"));
        }
    }
    // A leading `.` also covers the bare `.`/`..` traversal components; a leading
    // `-` would otherwise be mistaken for a flag downstream.
    if id.starts_with('.') || id.starts_with('-') {
        return Err(invalid(kind, id, "must not start with '.' or '-'"));
    }
    Ok(())
}

/// Build the rejection error, rendering the offending id with `{:?}` so a control
/// character is escaped rather than emitted raw.
fn invalid(kind: &str, id: &str, why: &str) -> Error {
    Error::Manifest(format!("invalid {kind} {id:?}: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_id_accepts_real_versions_and_names() {
        for id in [
            "1.5.0",
            "1.6.0-beta.2",
            "1.6.0-rc.1",
            "0.0.0-dev",
            "v1.5.0",
            "2024.1",
            "nightly",
            "vNext",
            // ids the loader itself passes through this validator:
            "runtime",
            "bun",
        ] {
            assert!(validate_id("version", id).is_ok(), "should accept {id:?}");
        }
    }

    #[test]
    fn validate_id_rejects_traversal_and_funny_bytes() {
        for id in [
            "",
            "..",
            ".",
            "../../etc",
            "/abs",
            "a/b",
            "a\\b",
            ".hidden",
            "-flag",
            "a\u{0}b",
            "foo/../bar",
        ] {
            assert!(validate_id("version", id).is_err(), "should reject {id:?}");
        }
    }

    #[test]
    fn validate_id_error_names_kind_and_id() {
        let err = validate_id("version", "../../etc").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("version"), "message names the kind: {msg}");
        // The id is rendered with {:?}, so it appears quoted.
        assert!(
            msg.contains("\"../../etc\""),
            "message quotes the id: {msg}"
        );
    }

    #[test]
    fn validate_id_does_not_echo_control_chars_raw() {
        let err = validate_id("version", "a\u{7}b").unwrap_err();
        let msg = err.to_string();
        // The bell byte must be escaped (\u{7}), never present verbatim.
        assert!(!msg.contains('\u{7}'), "raw control char leaked: {msg:?}");
    }
}
