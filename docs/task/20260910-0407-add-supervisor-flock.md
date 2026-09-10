# 20260910-0407-add-supervisor-flock Add a kernel lock to supervisor startup

- **status**: completed
- **priority**: P1
- **owner**: startup-lock/session-20260910
- **createdAt**: 2026-09-10 04:07

## Description

Add a nonblocking flock to the existing PID-file lock. The user explicitly approved adopting flock after the process identity fix.

Acceptance criteria:
- Only one new-version supervisor can acquire a data directory, including repeated acquisition in the same process and missing/corrupt PID metadata.
- Kernel ownership ends on normal drop and forced process exit, while the advisory lock file keeps a stable inode.
- A live legacy PID-only holder still blocks startup; dead or unrelated legacy holders are reclaimed using the existing identity check.
- Failed PID-file creation or writes release the kernel lock.
- Focused and relevant workspace/binary tests pass.

## ActiveForm

Completed kernel exclusion and lifecycle verification for supervisor startup.

## Dependencies

- **blocked by**: (none)
- **blocks**: (none)

## Approved Scope

- Standard tier; approved by the user's request to adopt flock.
- Use the existing nix::fcntl::Flock API with LockExclusiveNonblock on lode.pid.lock. Open without truncation and keep the file for the full LockGuard lifetime.
- Keep the existing PID identity checks and create_new acquisition for compatibility with already-running legacy instances.
- Remove lode.pid while still holding flock on normal drop; never unlink lode.pid.lock. Kernel ownership automatically ends even after SIGKILL.
- Retain the public read-only live_holder contract and the existing process identity probe.
- Limit implementation and tests to crates/lode-core/src/lock.rs, plus the existing changelog and task tracking. No dependencies or configuration changes.

## Notes

- Builds on completed task 20260910-0354-fix-stale-pid-lock; the prior changes remain uncommitted in the working tree.
- state.rs already uses a persistent sibling lock file, and nix has the required fs feature enabled.
- Establish RED for same-process reacquisition and corrupted PID metadata before implementation; verify cross-process and legacy compatibility afterward.

## Verification Progress

- RED: same-process reacquisition and damaged PID metadata incorrectly allowed another acquisition; the stable advisory lock file was absent (3 failing tests).
- GREEN: all 20 focused lock tests passed, with the subprocess helper invoked by lifecycle tests.
- Added Flock<File> ownership to LockGuard. PID metadata cleanup runs before field drop releases the kernel lock, including write/sync error paths.
- Legacy PID-only holders still block repeated acquisition attempts, and takeover succeeds after their exit.
- A missing/corrupt lode.pid cannot bypass an active new-version flock. The lode.pid.lock inode persists across normal release and restart.
- Local diff review found no new actionable issues. Workspace and binary integration checks passed.

## Final Verification

- Focused lock suite: 20 passed; the ignored helper runs in spawned subprocesses.
- Workspace nextest: 246 passed, 1 subprocess helper skipped as a standalone case.
- Doctests: 4 passed.
- Related binary end-to-end suite: 7 passed across 6 files.
- Formatting, Clippy, changed-file spelling, and git diff --check passed.
- The debug binary at target/debug/lode was rebuilt. No commits, pushes, or deployment were performed.
- Dependency manifests and the lockfile remain unchanged. The prior task records passing dependency-policy/MSRV checks and the pre-existing cargo-shear findings.

- complete: Kernel lock and legacy compatibility verified: 20 focused tests, 246 workspace tests, 4 doctests, 7 end-to-end tests, formatting, Clippy, spelling, and diff checks passed.
