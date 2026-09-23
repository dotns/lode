# 20260923-1113-merge-go-sdk-cross-platform Merge PR #1 and fix integration findings

- **status**: in_progress
- **priority**: P2
- **owner**: pr1-review/session-20260923
- **createdAt**: 2026-09-23 11:13

## Description

Review, fix, and merge PR #1 (`sdks(go): build on platforms without flock(2)`), then fix the bugs its author reported while integrating lode.

Acceptance criteria:
- The Go SDK builds and vets for linux, darwin, windows, and freebsd; Unix locking behavior is unchanged.
- SDK docs no longer describe the Go SDK as a single file.
- `lode.pid` contains only the pid, so `kill -TERM "$(cat lode.pid)"` works; legacy two-line files are still read.
- Relevant Rust and Go checks pass; PR #1 is merged into `main`.

## ActiveForm

Reviewing and merging PR #1 with follow-up fixes.

## Dependencies

- **blocked by**: (none)
- **blocks**: (none)

## Approved Scope

- Full tier; the user asked to review, fix, merge, and include the other reported bugs.
- See plan 20260923-1113-merge-go-sdk-cross-platform.

## Notes

- PR #1 compiles on all targets; the remaining `syscall.SIGTERM`/`SIGINT` use exists on Windows.
- `go get github.com/dotns/lode/sdks@latest` already resolves via a pseudo-version, so the missing tags are not a build blocker.

## Verification Progress

- PR #1 squash-merged as 7cd5223; the docs follow-up is a separate commit on main because pushing to the contributor fork was not permitted.
- Go SDK: `go vet` passes for linux, darwin, windows, and freebsd (amd64); `examples/go` builds for windows.
- RED: `acquire_creates_and_drop_releases` and `reclaims_lock_recording_own_pid` failed on the two-line `lode.pid`.
- GREEN: 20 lock tests pass; workspace `cargo test` passes (180 lode-core lib tests plus other crates); fmt, Clippy `-D warnings`, and `cargo build --bins --locked` pass on Rust 1.96.0.
- Under plain multi-threaded `cargo test`, two pre-existing tests that write and then exec a script (`runtime_version_probe_matches_and_rejects`, `replace_executable_swaps_in_the_probed_binary_and_keeps_its_mode`) fail intermittently. The cause looks like ETXTBSY. This is unrelated to this change, and CI's nextest runs each test in its own process.

