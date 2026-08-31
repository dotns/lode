//! External-crate smoke tests for `lode-core`'s public library API.
//!
//! An integration test compiles as a SEPARATE crate that can reach only the
//! *public* surface (no `crate::`-internal access) — proving the clap-free
//! [`lode_core::Config`] / [`lode_core::Engine`] pair is usable by an embedder.
//!
//! The helpers propagate `Result` rather than `unwrap`ing: clippy's
//! `allow-unwrap-in-tests` exempts only `#[test]` bodies, and an integration-test
//! crate is not built with `cfg(test)` — so the `unwrap()`s live in the tests.

use std::path::{Path, PathBuf};

/// A unique, fresh, empty temp dir for one test (std temp + a per-test tag and the
/// pid). Mirrors the in-crate tests' std-temp + pid pattern; the caller best-effort
/// removes it at the end.
fn fresh_dir(tag: &str) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("lode-api-smoke-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A sourceless, `policy = "off"` [`lode_core::Config`] rooted at `dir`, built clap-free
/// via the public [`lode_core::Config::from_toml`] — no clap/Globals, no update source,
/// no network.
fn sourceless_config(dir: &Path) -> lode_core::Result<lode_core::Config> {
    let toml = format!(
        "[global]\napp = \"smoke\"\ndir = \"{}\"\n[update]\npolicy = \"off\"\n",
        dir.display()
    );
    lode_core::Config::from_toml(&toml)
}

/// Criterion 4: build a [`lode_core::Config`] with NO clap/Globals and drive a READ-ONLY
/// [`lode_core::Engine`] method that succeeds on a fresh, empty data dir — proving the
/// engine API is usable from outside the crate.
#[test]
fn engine_read_only_from_public_api() {
    let dir = fresh_dir("engine").unwrap();
    let cfg = sourceless_config(&dir).unwrap();

    // `versions()` is purely local (enumerates `$LODE_DIR/versions/`, no network)
    // and returns Ok on an empty dir ("none installed") — the read-only proof.
    lode_core::Engine::new(cfg).versions().unwrap();

    let _ = std::fs::remove_dir_all(&dir);
}
