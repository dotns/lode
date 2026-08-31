//! Embed the supervise loop in a host process that owns its own signals: no
//! global signal handlers, no subreaper, no `flock`.
//!
//! The host feeds events through a channel signal source; this example seeds a
//! local version, lets the loop spawn and supervise it, then injects a SIGTERM
//! from a background thread — the loop stops the child gracefully and returns.
//!
//! ```bash
//! cargo run -p lode-supervisor --example embedded
//! ```

use std::time::Duration;

use lode_core::{Config, InitOptions};
use lode_supervisor::{SuperviseOptions, serve_embedded, signal_channel};

fn main() -> lode_core::Result<()> {
    InitOptions::new().log_level("info").install();

    let dir = std::env::temp_dir().join("lode-example-embedded");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let cfg = Config::from_toml(&format!(
        "[global]\napp = \"demo\"\ndir = \"{}\"\n\
         [update]\npolicy = \"off\"\n\
         [command]\nrun = \"./demo.sh\"\n\
         [supervise]\nrestart = \"off\"\nreadiness = \"none\"\n",
        dir.display()
    ))?;

    // Seed a local "release" so the loop has something to supervise (no manifest,
    // no download, no signature — the offline path `lode-cli seed` uses).
    let artifact = dir.join("demo.sh");
    std::fs::write(
        &artifact,
        "#!/bin/sh\necho '[app] serving'\nwhile true; do sleep 1; done\n",
    )?;
    lode_core::install::seed_local(&cfg, "1.0.0", &artifact, true)?;

    // The host owns the process: it decides what a signal means and when the loop
    // sees one. `send(15)` is a raw SIGTERM — no libc/nix dependency needed.
    let (tx, mut signals) = signal_channel();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let _ = tx.send(15);
    });

    // `host_owned()` => no subreaper, no single-instance lock, no global signal
    // dispositions; the loop runs entirely off the injected source.
    let code = serve_embedded(cfg, &mut signals, SuperviseOptions::host_owned())?;
    tracing::info!(?code, "supervise loop returned");

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
