//! lode management subcommands. `status` plus the three local (no-network)
//! commands land here; `update` arrives with its own L3.

pub(crate) mod restart;
pub(crate) mod rollback;
// `seed` is the cli-only offline-install command (driven by `run_tool`); the
// `Engine` facade never wraps it, so it would be dead code under `--features
// engine`. The other commands stay live — the facade calls them.
#[cfg(feature = "cli")]
pub(crate) mod seed;
pub(crate) mod status;
pub(crate) mod update;
pub(crate) mod versions;
