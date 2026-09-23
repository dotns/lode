# 20260923-1113-merge-go-sdk-cross-platform Merge PR #1 and fix integration findings

- **status**: in-progress
- **createdAt**: 2026-09-23 11:13
- **approvedAt**: 2026-09-23 11:13
- **relatedTask**: 20260923-1113-merge-go-sdk-cross-platform

## Context

PR #1 moves the Go SDK's `flock(2)` call behind build tags so the package compiles on Windows. The change is correct, but several docs still describe the Go SDK as one file. The PR also reports that `lode.pid` stores the app name on a second line, which breaks the conventional `kill "$(cat lode.pid)"`. The lock reader only parses the first line and never reads the app name.

## Proposal

1. Push a docs fix to the PR branch covering the stale single-file wording, then squash-merge PR #1. Verify `go vet` for linux, darwin, windows, and freebsd.
2. Write only the pid to `lode.pid` and remove the now-unused `app` parameter from `lock::acquire`. Keep reading legacy two-line files. Update the lock tests (RED first), architecture docs, and changelog.
3. Run the lock tests, the workspace tests, Clippy, and formatting, then push to `main` and confirm CI.

## Risks

An older lode reading a new pid-only file still parses the first line. Nothing else reads the app name. Some external scripts may parse the second line, but it has never been documented as a contract beyond the architecture note.

## Scope

sdks/, examples/README.md, crates/lode-core/src/lock.rs, crates/lode-supervisor/src/lib.rs, docs/architecture*.md, CHANGELOG.md, and tracking files. The `check_nonce` and `last_request_result` suggestions are state.json contract features, so they are out of scope here.

## Alternatives

Documenting the two-line format instead would keep a nonstandard pid file for no benefit.

## Annotations

The user explicitly asked to review, fix, and merge PR #1, including the other bugs, so no further approval is needed for this scope.

## Results

