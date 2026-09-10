# 20260910-0354-fix-stale-pid-lock Fix stale PID locks blocking startup

- **status**: completed
- **priority**: P1
- **owner**: startup-lock/session-20260910
- **createdAt**: 2026-09-10 03:54

## Description

Prevent stale lode.pid files from blocking startup when the recorded PID has been reused by an unrelated process. Preserve exclusion of concurrent supervisors sharing a data directory and consistent live_holder detection.

Acceptance criteria:
- An unrelated live process recorded in a stale PID file does not prevent startup.
- A genuinely active lock holder prevents a second acquisition.
- Dead, corrupt, and reused PID records recover without disrupting another active holder.
- Focused regression tests and relevant workspace checks pass.

## ActiveForm

Completed process identity checks and regression verification for stale PID locks.

## Dependencies

- **blocked by**: (none)
- **blocks**: (none)

## Notes

- Standard tier: implementation is expected within crates/lode-core/src/lock.rs, including unit tests; concurrency semantics exclude the trivial tier.
- The current implementation calls kill(pid, None), but treats any live process as the lock owner. Existing tests explicitly accept a sleep child as a holder.
- The reported /app/bkd installation is unavailable in this workspace; the identity of PID 64 and installed binary version remain unverified.
- Supervisor startup calls lock::acquire; commands/update.rs uses lock::live_holder for hot-update routing.
- The user approved implementation with a correction: retain PID files and verify that the recorded process is actually lode. Tracking is file-only.

## Approved Scope

- Keep the existing PID-file acquisition and release mechanism.
- On Linux/Android, inspect /proc/<pid>/exe after the signal-zero probe. Accept the current executable (including renamed installations) and the distributed lode/lode-cli executable names. Normalize the kernel's deleted-executable suffix for in-place upgrades.
- Reclaim records pointing at unrelated or exited processes. Preserve locks conservatively when executable identity cannot be read because of permissions or another inspection error.
- Use the same identity check in acquire and live_holder; reject nonpositive PID values as corrupt metadata.
- Preserve the existing conservative liveness fallback on platforms without Linux procfs.
- Keep implementation and regression tests in crates/lode-core/src/lock.rs, with no new dependencies.

## Decision History

- The initial proposal used a persistent advisory lock. The user instead requested verification of the recorded process identity, so advisory-lock migration is outside the approved scope.

## Validation

- Baseline: cargo test --locked -p lode-core lock::tests -- --nocapture passed all 9 tests on Rust 1.96.0.
- Existing refuses_lock_held_by_live_other_process and live_holder_reports_live_other_process tests deliberately write a sleep child PID, demonstrating the incorrect assumption directly.
- Establish RED for stale metadata pointing at a live unrelated process, then implement minimally and verify genuine cross-process contention, forced-exit recovery, invalid/dead/corrupt PID records, and live_holder consistency.
- Run focused tests, workspace nextest, doctests, formatting, and Clippy after implementation.

## Implementation Progress

- RED: four regression tests failed before the fix: unrelated PID reclamation, read-only unrelated-holder detection, nonpositive PIDs, and an unreaped killed holder. The startup regression returned `another lode instance is already running`; zombie and unrelated-holder probes incorrectly returned `Some(pid)`.
- GREEN: all 15 lock tests pass; the ignored subprocess helper is invoked by the contention and forced-exit tests.
- Workspace formatting, Clippy, nextest, doctests (4), and the debug binary build passed.
- Changed-file spelling checks passed. cargo shear reports the pre-existing `unused dependency serde_json` in crates/lode-supervisor/Cargo.toml and two doctest configuration warnings; no dependency manifests were changed.
- Diff review: executable checks use procfs rather than argv[0], preserve renamed/current and distributed executable paths, normalize deleted executable suffixes, retain conservative behavior on identity-read errors, and keep live_holder read-only. No new actionable findings.
- All 7 related binary end-to-end tests passed across bootstrap, graceful stop, live auto-update, keep-alive recovery, paused recovery, and configuration reload. Linux/Android gain executable identity checks; other Unix targets retain the existing signal-zero fallback.

## Final Verification

- cargo deny --locked check: passed advisories, bans, licenses, and sources; existing configuration/duplicate-dependency warnings remain.
- cargo hack check --rust-version --workspace --locked: passed all three crates on Rust 1.96.
- git diff --check: passed.
- cargo shear: existing `unused dependency serde_json` in lode-supervisor and two `doctest_enabled_without_doctests` warnings remain outside this fix.
- The debug binary was rebuilt at target/debug/lode. No changes were deployed to the unavailable /app/bkd installation.

- complete: PID identity fix verified with 15 focused tests, workspace tests, 4 doctests, 7 binary end-to-end tests, formatting, Clippy, spelling, cargo-deny, and MSRV checks. Existing cargo-shear findings are recorded separately.
