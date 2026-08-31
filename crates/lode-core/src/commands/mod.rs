//! lode management subcommands. `status` plus the three local (no-network)
//! commands land here; `update` arrives with its own L3.

pub mod restart;
pub mod rollback;
// `seed` is the cli-only offline-install command (driven by `run_tool`); the
// `Engine` facade never wraps it, so it would be dead code under `--features
// engine`. The other commands stay live — the facade calls them.
pub mod seed;
pub mod status;
pub mod update;
pub mod versions;
