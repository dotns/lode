//! Embed the clap-free core: build a [`Config`] in code, seed a local version, and
//! drive the read-only [`Engine`] facade — no CLI, no signals, no network.
//!
//! ```bash
//! cargo run -p lode-core --example engine
//! ```

use std::path::Path;

use lode_core::{Config, Engine, InitOptions};

fn main() -> lode_core::Result<()> {
    // A library consumer opts into process-global effects explicitly; here, just
    // logging (no crypto provider needed — this example never touches the network).
    InitOptions::new().log_level("info").install();

    let dir = std::env::temp_dir().join("lode-example-engine");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    // Sourceless + policy=off: nothing is fetched, so the example is offline and
    // deterministic. `ConfigBuilder` fills the same override slot the CLI does.
    let cfg = Config::builder()
        .app("demo")
        .dir(dir.to_string_lossy())
        .policy(lode_core::Policy::Off)
        .run("./demo.sh")
        .build()?;

    // Seed a local "release" the way `lode-cli seed` does: no manifest, no
    // download, no signature — the offline path for tests and demos.
    let artifact = dir.join("demo.sh");
    std::fs::write(&artifact, "#!/bin/sh\necho demo app\n")?;
    lode_core::install::seed_local(&cfg, "1.0.0", Path::new(&artifact), true)?;

    // Read-only facade calls: what is installed, and what would run right now.
    let engine = Engine::new(cfg);
    engine.versions()?;
    tracing::info!(target = %engine.resolve_target()?, "resolved launch target");

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
