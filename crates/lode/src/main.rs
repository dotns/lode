#![forbid(unsafe_code)]
//! Binary entry point for `lode`. The implementation lives in the `lode` library
//! crate (`src/lib.rs`); this shim only maps the result to a process exit code.

use std::process::ExitCode;

fn main() -> ExitCode {
    match lode::run() {
        Ok(code) => code,
        Err(err) => {
            tracing::error!("lode failed: {err:#}");
            ExitCode::FAILURE
        }
    }
}
