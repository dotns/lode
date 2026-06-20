# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.0.6] - 2026-06-16

### Changed (behavioral)

- **`[supervise].restart_backoff` and `restart_backoff_max` are now in SECONDS, not milliseconds.**
  Defaults change accordingly: `restart_backoff` `500` (ms) → `1` (second); `restart_backoff_max`
  `30000` (ms) → `30` (seconds). The crash-restart backoff sequence is now `1s, 2s, 4s, 8s, 16s, 30s(cap)`
  (previously `0.5s, 1s, 2s, …`). **Breaking:** an existing config with `restart_backoff = 500` now means
  500 *seconds*, not 0.5s — update such values to seconds. This unifies every `[supervise]`/`[update]`
  time field on seconds (the only remaining sub-second values are internal loop-tick constants, not config).
  CLI flags `--restart-backoff`/`--restart-backoff-max` and `LODE_RESTART_BACKOFF*` now take seconds.

## [0.0.5] - 2026-06-11

### Changed (behavioral)

- **`[supervise].restart` default flipped `off` → `on-failure`, and `restart_max` default `0` → `3`.**
  A failing app is now retried (exponential backoff) and, after `restart_max` failures, lode
  **pauses** — PID 1 stays alive with `status = "error"` — instead of exiting and crash-looping
  the container. Recover a paused app without an exit: edit `lode.toml` (the file is watched
  while paused; a running app is never disturbed by edits), bump `restart_nonce`, or write a new
  `target`. Set `restart = "off"` to restore the old mirror-the-child behavior.
- Keep-alive supervisor + staged-update prepare handshake (`state.ready` phased
  `{LODE_INSTANCE}-{0|1|2}`, app-paced cut-over) and verified per-version download cache
  (carried from the unreleased commits since 0.0.4).
- Removed the `entry` concept entirely: `[update].entry`, `--entry`/`LODE_ENTRY`, `{entry}` placeholder, and the advisory in-archive entry field are gone (no backward compatibility — 0.0.5 is unreleased).
- `[command].run` and `[command].exec` are now **literal** launch commands (whitespace-split, cwd = version dir). Only the `{dir}` template remains. Neither is required at config-parse time — a manifest asset may supply `run`/`exec` overrides instead.
- A manifest asset may publish optional `run` and `exec` fields. When present they override the operator's `[command].run`/`exec`. These fields are bound into the artifact signature and the catalog signature, so they are tamper-evident under `require_signature = "auto"` (with keys) or `enforce`.
- lode auto-chmod+x the first whitespace token of the effective run (and exec, if different) when it names a relative path resolving to a file inside the version dir.
- Launch fails with a clear hard error (`no run command: set [command].run or publish \`run\` in the manifest asset`) when neither `[command]` nor the manifest supplies a run command.
- `lode-cli sign`/`manifest`/`manifest-sign` now accept optional `--run`/`--exec` to publish launch overrides with assets.
- The per-artifact signed message now includes `run` and `exec` fields (empty string when absent). **Breaking change to signature format** — re-sign all assets when upgrading from 0.0.4 signatures.
- The `.lode.json` marker stores `run`/`exec` (manifest-supplied overrides) instead of `entry`, enabling offline relaunches with the correct override.
- Scaffolded starter `lode.toml` (written on first run / `lode-cli init`) is now minimal; `docs/lode.example.toml` is the full annotated reference.

### Fixed

- P0-1: corrupt/torn `state.json` no longer kills the supervisor (lenient reads + quarantine to
  `state.json.corrupt`) — previously a persistent PID-1 crash-loop.
- P0-2: a `lode.pid` recording lode's own pid (PID-1 restart after `kill -9`/OOM on a persistent
  volume) is reclaimed as stale instead of self-deadlocking.
- P0-3: `target: "latest"` (the documented app contract) now resolves through channel-latest
  before apply, on both the hot-update and update-on-exit paths.
- P0-4: the `lode serve` CLI doc-comment claimed the restart default was `off`; corrected to
  `on-failure` (doc-comment only — the behavioral flip itself is the Changed entry above).
- P1-5: all HTTP fetches now run through a timeout-configured agent (connect 10s; bounded
  response/body phases) — a hung server can no longer freeze the supervise loop.
- P1-6: spawn/exec failures during update/rollback now roll back / pause instead of exiting PID 1.
- P1-7: supervise-loop `state.json` writes are best-effort (disk-full/read-only no longer kills
  the supervisor; pause works without a writable disk).
- P1-8: the starter `lode.toml` no longer ships uncommented `${RELEASE_TOKEN}`/`${API_KEY}`
  headers (first run works out of the box).
- P1-9: unknown `lode.toml` keys are now rejected (`deny_unknown_fields`) — typos fail loudly
  instead of silently no-opping.
- P2-10: custom `[http].headers` are stripped on cross-host redirects (manual redirect loop,
  5-hop cap, per-hop allowlist + scheme enforcement).
- P2-11: `policy = "auto"` no longer re-applies a version whose last observation failed
  (bad-version history consulted).
- P2-12: a paused app whose recovery `target` fails to install stays paused (`lode.toml`-edit
  recovery keeps working).
- P2-13: `restart_nonce` now acts in every update phase; optional `[supervise].prepare_timeout`
  (default 0 = app-paced) force-cuts-over a never-acking app.
- P2-14: lode-side `state.json` read-modify-writes serialize via flock on `state.json.lock`;
  the readiness token can no longer be clobbered post-spawn.
- P2-15: `lode-cli update` detects a paused/backing-off supervisor (via the instance lock) and
  hands off through `state.target` instead of flipping `current` underneath it.
- P2-16: the app child runs in its own process group; stop/forward signal the group (fork-model
  workers no longer survive updates).
- P2-17: signal handlers are installed before bootstrap/runtime downloads (`docker stop` works
  during a long bootstrap).
- P2-19: documented that an unset `${VAR}` in `lode.toml` is a hard startup error, and added the
  runtime-downloads-are-TLS-only note.
- P3-20: `lode-cli keygen` writes the private key 0600.
- P3-22: `[global].log_level` now takes effect (CLI/env > TOML > default).
- P3-23: TOML parse errors no longer echo file content (secret-safe); empty
  `app`/`channel`/`asset` rejected; docker `latest` retagging gated; Dockerfile binary COPY
  deduped.

### Security / Release

- CI/release/docker workflows: actions pinned to commit SHAs; `SHA256SUMS` published with
  releases; unsigned releases fail loud unless explicitly allowed; base image digest-pinned
  (P2-18).
- `SECURITY.md` expanded with a threat-model summary; runtime downloads documented as TLS-only
  (no hash/signature verification) — see [Known limitations](#known-limitations) and
  `SECURITY.md` (P3-21).

### Internal / Testing

- New regression e2es: corrupt-state boot/mid-run, `target = "latest"` hot + exit paths,
  spawn-failure rollback, paused-recovery, prepare-timeout; the compose suite gained
  per-worktree isolation (concurrent-run safe).
- `state.json` concurrency contract documented in `docs/integration.md` (apps SHOULD flock
  `state.json.lock` for read-modify-writes).

### Known limitations

- `[runtime].download` artifacts are still TLS-only — not hash- or signature-verified
  (see `SECURITY.md`).
- Boot-path state read-modify-writes (`bootstrap_terminated` / clearing stale `ready`) are
  intentionally unserialised — they run before the child exists (low risk).
- `lode-cli seed` (with activation) orders its symlink flip before its strict state read
  (CLI-only wart).
- (deferred) `clear_target` unconditional clear vs a raw `latest` alias: `clear_target` always nulls `state.target` after processing an update, but when the request was for the raw string `"latest"` (not a resolved version), a failed install leaves the target cleared — the app would need to re-request to retry. Correct fix threads the raw request through `pending_update`/`exit_action`/`on_child_exit`; a value-compare shortcut would leave failed `latest`-requests unconsumed and cause endless install-retry.
- (deferred) `write_pre_observe_state` unconditional target-null at cut-over: same race class — nulls `st.target` unconditionally at cut-over, which can clear a concurrently-written target request.

## [0.0.4]

`require_signature` now gates artifacts only; the catalog/manifest signature became
verify-if-present (never required); client downgrade floor on `latest`
(`max(current, last_good)`); new `lode-cli seed` for offline local-version installs.
See the [v0.0.4 release notes](https://github.com/dotns/lode/releases/tag/v0.0.4).

Older releases (0.0.1–0.0.3): see the [GitHub releases](https://github.com/dotns/lode/releases).

[0.0.5]: https://github.com/dotns/lode/compare/v0.0.4...v0.0.5
[0.0.4]: https://github.com/dotns/lode/releases/tag/v0.0.4
