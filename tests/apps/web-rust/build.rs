// build.rs — bake the app's version (and an optional "bad" flag) into the binary.
//
// This is how distinct v0.0.1 / v0.0.2 / crashing-v0.0.3 artifacts are produced
// from one source tree:
//
//   BUILD_VERSION=0.0.1            cargo build --release   # -> reports 0.0.1
//   BUILD_VERSION=0.0.2            cargo build --release   # -> reports 0.0.2
//   BUILD_VERSION=0.0.3 BUILD_BAD=1 cargo build --release  # -> crashes on start
//
// Version source precedence: env `BUILD_VERSION` > a `VERSION` file next to
// Cargo.toml > "0.0.0-dev". At runtime, lode's injected `LODE_ACTIVE_VERSION`
// still wins over this baked value (see src/main.rs).
use std::env;
use std::fs;

fn main() {
    println!("cargo:rerun-if-env-changed=BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=BUILD_BAD");
    println!("cargo:rerun-if-changed=VERSION");

    let version = env::var("BUILD_VERSION")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            fs::read_to_string("VERSION")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "0.0.0-dev".to_string());
    println!("cargo:rustc-env=BUILD_VERSION={version}");

    let bad = env::var("BUILD_BAD").map(|v| v == "1").unwrap_or(false);
    println!("cargo:rustc-env=BUILD_BAD={}", if bad { "1" } else { "0" });
}
