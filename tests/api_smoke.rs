//! External-crate smoke tests for lode's public library API.
//!
//! These live in `tests/` so they compile as a SEPARATE crate that can reach only
//! lode's *public* surface (no `crate::`-internal access) — proving the clap-free
//! [`lode::Config`]/[`lode::Engine`] and the host-owned, signal-injectable
//! supervise entry are usable by an embedder. Cargo only builds top-level
//! `tests/*.rs`, so this file is isolated from the bun e2e suite that shares the
//! `tests/` directory.
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

/// A sourceless, `policy = "off"` [`lode::Config`] rooted at `dir`, built clap-free
/// via the public [`lode::Config::from_toml`] — no clap/Globals, no update source,
/// no network.
fn sourceless_config(dir: &Path) -> lode::Result<lode::Config> {
    let toml = format!(
        "[global]\napp = \"smoke\"\ndir = \"{}\"\n[update]\npolicy = \"off\"\n",
        dir.display()
    );
    lode::Config::from_toml(&toml)
}

/// Criterion 4: build a [`lode::Config`] with NO clap/Globals and drive a READ-ONLY
/// [`lode::Engine`] method that succeeds on a fresh, empty data dir — proving the
/// engine API is usable from outside the crate.
#[test]
fn engine_read_only_from_public_api() {
    let dir = fresh_dir("engine").unwrap();
    let cfg = sourceless_config(&dir).unwrap();

    // `versions()` is purely local (enumerates `$LODE_DIR/versions/`, no network)
    // and returns Ok on an empty dir ("none installed") — the read-only proof.
    lode::Engine::new(cfg).versions().unwrap();

    let _ = std::fs::remove_dir_all(&dir);
}

/// Criterion 5: construct a HOST-OWNED signal source and drive the embeddable
/// supervise entry with an INJECTED SIGTERM — proving the loop runs off the
/// injected source, with no `signal_hook::Signals::new` and no process-global
/// signal dispositions installed.
#[test]
fn serve_embedded_host_owned_terminates_on_injected_sigterm() {
    let dir = fresh_dir("serve").unwrap();
    let cfg = sourceless_config(&dir).unwrap();

    let (tx, mut src) = lode::signal_channel();
    tx.send(15).unwrap(); // 15 = SIGTERM (raw signal number; no libc/nix dep)

    // `host_owned()` => no subreaper, no flock, no global signal dispositions. The
    // pending SIGTERM is observed by the bootstrap-termination check before any
    // child spawn or version resolution, so no installed version is required and
    // the loop returns Ok via the INJECTED source alone.
    let _code = lode::serve_embedded(cfg, &mut src, lode::SuperviseOptions::host_owned()).unwrap();

    let _ = std::fs::remove_dir_all(&dir);
}
