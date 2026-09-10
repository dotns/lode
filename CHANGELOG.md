# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.2] - 2026-09-10

### Fixed

- On Linux/Android, verify the executable behind an existing `lode.pid` before
  treating it as a running lode instance. Reclaim stale files when the PID belongs
  to an unrelated executable or an unreaped exited process, and reject nonpositive
  PIDs. Startup and running-supervisor detection use the same check.
- Hold a nonblocking kernel lock on `lode.pid.lock` throughout supervision to
  prevent concurrent starts even when PID metadata is missing or corrupt. The
  kernel releases ownership after a forced exit; existing PID identity checks
  preserve compatibility with running legacy instances.

## [0.3.1] - 2026-09-08

### Fixed

- **GitHub source: an unlabelled asset is unsigned, not signed with the empty
  string.** The API returns `"label": ""` — not `null` — for an asset uploaded
  without a label, and the adapter carried that through as a present signature.
  That both suppressed the `<name>.sig` sidecar fallback added in 0.3.0 and made
  verification fail on the empty string with a misleading "did not match any
  trusted key", so `lode-cli self-update` (and any `github`-source app whose
  release signs via sidecars) could not install the 0.3.0 release. A blank label
  now decays to "no signature", restoring the sidecar fallback.

## [0.3.0] - 2026-09-08

### Added

- **`lode-cli self-update`.** Replaces the running lode binary with the newest stable
  lode release (or `--version <tag>`, also the deliberate-downgrade path) from
  `github.com/dotns/lode`, verified under `require_signature = enforce` against the
  release keys compiled into the binary (`crates/lode/release-keys.txt`;
  `--release-key <entry>` overrides them for key-rotation recovery). It never reads the
  app's `lode.toml` / `LODE_*` source or trusted keys. The new binary is probed
  (`--version` must report the target) and atomically renamed over the executable;
  a running supervisor keeps the old binary until lode restarts.
- **GitHub source: `.sig` sidecar assets.** When a release asset carries no `label`,
  the adapter reads its signature from a `<name>.sig` asset of the same release (a
  label still wins). Publishers can keep the release page readable — GitHub shows a
  label *in place of* the filename — by uploading the signature as a sidecar instead.
- **Selectable signature algorithm.** Publisher keys now carry an `alg`:
  `ed25519` (unchanged default), `ecdsa-p256` (SHA-256, 64-byte `r||s` signature) or
  `ecdsa-p384` (SHA-384, 96-byte `r||s`). `lode-cli keygen --alg <alg>` generates the
  key; non-ed25519 keys are tagged `<alg>:…` in the key file, the `.pub` line and the
  `trusted_keys` entry (`<alg>:<key_id>:<base64>`), and `--key` / `--key-env` /
  `--pubkey` accept the tagged form. ECDSA public keys are SEC1 (compressed canonical;
  uncompressed accepted), `key_id` is derived over the compressed encoding.
- **The algorithm is pinned on the trusted key, never read from the manifest.** Each
  `[trust].trusted_keys` entry is verified with its own algorithm; key bytes relabelled
  under another algorithm fail to decode. The manifest gains an optional, advisory
  `alg` field (top level and per asset, absent = `ed25519`) that `lode-cli manifest` /
  `manifest-sign` stamp for non-ed25519 keys; unknown names are logged, not fatal.
  The signed messages (`lode.artifact.v1` / `lode.manifest.v1`) are unchanged, so
  existing ed25519 keys, signatures and manifests keep verifying as before.
- `lode-cli keygen` / `sign` print an `alg:` line; `lode_core::verify` exposes
  `Algorithm`, `PublicKey` and `decode_trusted_key`. `verify::key_id` now takes
  `&[u8]` (any canonical public-key encoding), and `verify_signature` accepts a
  tagged trusted-key entry in place of a bare base64 key.

### Changed

- **`lode-cli manifest` now requires `--app <name>` (or `LODE_APP_NAME`) and `--url`.**
  The manifest `name` must equal the loader's `[global].app`, so the former silent
  default `"app"` published a catalog every instance rejected; `--url` no longer
  defaults to the placeholder `https://...`.
- Documentation sweep against the 0.2.0 implementation: README quick-start gains the
  mandatory `[update].asset`; library usage points at git tags (the crates are not on
  crates.io); `held`/`hold` and `ready` join the `state.json` spec; rollback-target
  failure pauses (keep-alive) rather than exiting; the injected child env lists
  `LODE_WORKDIR`/`LODE_CONFIG`/`LODE_READINESS`; the signed message is quoted with
  `run`/`exec` everywhere; the default forward-signal set, runtime cache path
  (`runtime/<key>/<name>`), `SIGCHLD`/fd-passing/offline wording, `SECURITY.md`
  paths and versioning, and the test READMEs now match the code. The two deferred
  `target`-request races from 0.0.5 are recorded under `SECURITY.md` *Known
  Limitations*.
- e2e coverage: scenario 28 (signal passthrough — default forward set, a narrowed
  `--forward-signals`, and a consumed `--restart-signal`) and scenario 29 (the GitHub
  Releases source, driven against a local API stand-in, `tests/src/helpers/githubServer.ts`);
  `tests/README.md` now lists what deliberately stays unit-level.

### Changed

- **Keys and signatures are printed as base64url.** `lode-cli keygen` / `sign` /
  `manifest` / `manifest-sign` now emit unpadded base64url (RFC 4648 §5: `-` `_`
  instead of `+` `/`, no `=`), so public keys, seeds and signatures are safe in URLs,
  filenames and shells. Every decoder accepts both alphabets, padded or not, so
  existing `trusted_keys` entries, `LODE_SIGNING_KEY` secrets, `.sig` files and
  manifests keep working. Loaders older than this release decode standard base64
  only: a publisher whose operators still run them should keep signing with an older
  `lode-cli` until those hosts upgrade.
- **Release assets are signed via `.sig` sidecars.** `.github/workflows/release.yml`
  uploads `lode-<os>-<arch>.tar.gz` plus `lode-<os>-<arch>.tar.gz.sig` instead of
  carrying the signature in the asset label, so the Releases page shows real filenames
  again. The GitHub recipes in `docs/source-adapters.md` and `docs/integration.md`
  follow suit.

### Fixed

- **GitHub source: one version id per release.** A `pin` (or `update --version`) given
  as the raw tag (`v1.5.0`) now resolves to the same `v`-stripped id (`1.5.0`) that
  following `latest` yields, instead of registering the raw tag as a second version.
  `versions/<id>` and the signed `version` therefore no longer depend on how the
  release was selected, and one signature — made over the stripped id — verifies on
  both paths. The GitHub release recipes (and lode's own workflow) signed with the raw
  tag (`--version "$TAG"`), which only ever verified for a pinned install; they now
  sign `${TAG#v}`. A `versions/v1.5.0` directory from an earlier pinned install keeps
  being used as is.

### Dependencies

- `p256` / `p384` 0.13 (RustCrypto, pure Rust; `ecdsa` + `std` features only), kept on
  the digest 0.10 generation shared with `ed25519-dalek` 2 / `sha2` 0.10 — the 0.14
  line requires `sha2` 0.11 and is deferred to a coordinated crypto-stack bump.

## [0.2.0] - 2026-08-31

### Changed (breaking, library only)

- **lode is now a three-crate workspace** (`crates/lode-core`, `crates/lode-supervisor`,
  `crates/lode`) and the Cargo feature gates (`engine`/`supervisor`/`cli`) are retired —
  the layer split they expressed is now expressed as crates. Migration for library
  consumers:
  - `lode` with `--features engine` → depend on **`lode-core`** (clap-free, signal-free:
    `InitOptions`, the `Engine` facade, `Config::from_toml` / `ConfigBuilder`).
  - `lode` with `--features supervisor` → depend on **`lode-supervisor`** (embeddable
    `serve_embedded` / `serve_core` / `exec_passthrough` + `SignalSource`; pulls in
    `lode-core`).
  - The `cli` feature is gone — the CLI **is** the `lode` crate, the workspace's only
    binary. The clap-bound config loaders moved to `lode/src/config_cli.rs`, and the
    enum-valued flags now use `ValueEnum` mirrors there, so `lode-core` never sees clap.
  - **Binary users are unaffected**: the same single `lode` / `lode-cli` binary, the same
    CLI (`--help` is byte-identical), zero behavior change; `cargo build` at the repo root
    still emits `target/debug/lode` (and `--profile dist` → `target/dist/lode`).

### Added

- **Library usage examples.** `cargo run -p lode-core --example engine` (config built in
  code + a seeded local version + the read-only `Engine` facade) and
  `cargo run -p lode-supervisor --example embedded` (the supervise loop embedded in a host
  process with host-owned signals — no global signal handlers, no subreaper, no flock).
  Both READMEs gain a "Use as a library" section.

### Fixed

- **A single failed fetch no longer surfaces to the app as `state.last_error`.** The
  periodic update check (and an auto-applied target's download) wrote `last_error` on
  every failed attempt, and nothing cleared it on the next success — so one flaky hop
  left a stale "download failed" in `state.json` for the app to consume indefinitely.
  Network-layer failures (`Error::Http`/`Error::Download` — connect/TLS/redirect/status,
  or a body that did not arrive intact) are now reported only after **3 consecutive**
  failures, and the next successful fetch clears exactly that report. Failures that will
  not fix themselves (malformed manifest, bad signature, full disk) are still reported on
  the first strike, and an explicit CLI `update` still reports its own failure directly.

- **`301`/`308` redirect coverage.** The manual redirect loop already followed every
  redirect status; a regression test now pins the permanent ones (a moved download URL),
  asserting the hop is taken and the second hop is a plain `GET` of the `Location` path.

## [0.1.1] - 2026-08-29

### Fixed

- **A runtime download is now cached per configured runtime, not per name.** The
  `[runtime]` cache moved from `$LODE_DIR/runtime/<name>` to
  `$LODE_DIR/runtime/<key>/<name>`, where `<key>` always carries a 12-hex digest of
  `[runtime].download`, prefixed by the pinned `[runtime].version` when it is a safe
  path component (`1.1.38-9f2c0a4b6d18`, else `url-9f2c0a4b6d18`). Repointing
  `download`/`version` at a new runtime version used to hit the old flat cache: lode
  logged "runtime served from cache; skipping download" and launched the app with the
  *previous* runtime (and when a `version` pin did force a re-fetch, the new archive
  was unpacked over the old tree, whose binary already sat at `runtime/<name>` — so
  hoisting was a no-op and the stale binary survived, failing the post-download version
  check). Now a changed `[runtime]` yields a new key, the payload is extracted into a
  freshly emptied directory, and the other `runtime/` entries — including the flat
  binary left by an older lode, which is re-downloaded once on upgrade — are reclaimed.

## [0.1.0] - 2026-07-01

### Added

- **lode is now consumable as a library.** The crate is split behind Cargo features
  (`default = ["cli"]`, plus `engine`, `supervisor = ["engine"]`, `cli = ["supervisor"]`) so
  downstream code can depend on lode's internals without pulling in the CLI:
  - **Layer 0 — opt-in process setup.** The four process-global installs (the rustls crypto
    provider, core-dump suppression, the tracing subscriber, and the panic hook) now live
    behind `InitOptions`; a library consumer gets none of them unless it opts in. The `lode`
    / `lode-cli` binary still installs all four, in the same order, at the same time.
  - **Layer 1 — `Engine` facade.** A public, clap-free / signal-free `Engine`
    (`status` / `check` / `install` / `rollback` / `versions` / `restart` /
    `resolve_target` / `ensure_runtime`) over the version + runtime machinery, moved into a
    new `src/engine.rs`. Public `Config::from_toml` + `ConfigBuilder` construct configuration
    without the clap layer. `--features engine` builds with neither clap nor signal-hook.
  - **Layer 2 — embeddable supervisor.** Public `Supervisor` / `serve_embedded` /
    `exec_passthrough` plus a `SignalSource` abstraction with **owned** (signal-hook) and
    **host-owned** (the caller feeds signal events) modes; the subreaper and the
    single-instance flock are skippable via `SuperviseOptions::host_owned()`, so a host
    process can embed the supervise loop without lode taking over process-global signal
    handling.

### Unchanged

- **Zero runtime-behavior change to the `lode` / `lode-cli` binary** — launch, supervise,
  update, rollback, and signal handling are byte-for-byte identical. Verified across 203 unit
  tests, 36 e2e scenarios, the docker-compose integration test, and the full `--features`
  build matrix (`default` / `engine` / `supervisor`).

## [0.0.10] - 2026-06-24

### Added

- **lode injects `LODE_CONFIG`** — the path to the `lode.toml` it loaded — so the app can read
  lode's config **read-only** (it's the operator's file; the app's write channel stays
  `state.json`). Not injected when running file-less (no config). Joins `LODE_DIR` /
  `LODE_WORKDIR` / `LODE_ACTIVE_VERSION` / `LODE_INSTANCE` / `LODE_READINESS`.
- **SDKs gain read-only config access**: `configPath()` (the path) and `readConfig()` (raw
  `lode.toml` text — parse with your own TOML lib if you need fields; keeps the Go SDK
  stdlib-only and the others dependency-light). The demo apps add a `GET /config` endpoint
  returning the path + raw config.

## [0.0.9] - 2026-06-24

### Changed (breaking)

- **Directory variables renamed for a clear app-vs-lode split.** lode's own directory is
  now `[global].dir` / `--dir` / **`LODE_DIR`** (was `data_dir` / `--data-dir` /
  `LODE_DATA_DIR`), and the app's run directory is `[command].workdir` / `--app-dir` /
  **`LODE_WORKDIR`** (was `workdir` / `--workdir` / `LODE_WORKDIR`). Update `lode.toml`
  (`[global].dir`, `[command].workdir`) and any `LODE_DATA_DIR` / `LODE_WORKDIR` env or
  `--data-dir` / `--workdir` flags. No silent aliases — old names are rejected.

### Added

- **lode now injects `LODE_WORKDIR`** (the app's run dir, i.e. its cwd) into the child,
  alongside `LODE_DIR` / `LODE_ACTIVE_VERSION` / `LODE_INSTANCE` / `LODE_READINESS`.
- **App directory convention (docs + SDKs).** Recommended split: lode provides `LODE_DIR`
  (its persistent dir, where state.json/versions/runtime live) and `LODE_WORKDIR` (the run
  dir); your **app** implements its own `ROOT_DIR` / `DATA_DIR`, resolving its data dir
  **`DATA_DIR` > `LODE_DIR` > `ROOT_DIR`** so the same binary works with or without lode
  (set only `ROOT_DIR` standalone; lode supplies `LODE_DIR` automatically). The SDKs gain
  `dataDir()` (the resolution), `rootDir()`, `lodeDir()`, `workdir()`; the `Lode` handle now
  keys off `LODE_DIR`. See docs/integration.md → "Data directories & persistence".

## [0.0.8] - 2026-06-23

### Added

- **App-requested hold (design §7): the app (or an operator, via `state.json`) can ask lode NOT to
  (re)start the process.** A new app-owned `state.json` field **`hold`** (bool): set it `true` and lode
  refrains from spawning — at boot, after the child exits, and on a `restart_nonce`/`target` request
  — and reports a new lifecycle status **`held`** while waiting. Clearing it (`false`) resumes a normal
  start. Use it for planned maintenance that must complete before the app comes up, e.g. a DB migration
  needing CLI intervention. lode polls `state.json`'s mtime and applies the flag (~1s); a hold present
  at boot is honoured before the first spawn.
- **SDKs (`sdks/`) gain `hold()` / `release()`** (set/clear the flag), the `held` status, and a `watch`
  `onHold` change callback.

### Behavior notes

- A hold gates *starts*, not a running child: lode does **not** kill an already-running process when
  `hold` is set — the app exits itself if maintenance needs the process down (e.g. set `hold` then
  `exit(0)`; lode then holds instead of respawning). A child exit racing the flag write is still caught
  (lode re-reads `hold` fresh on exit).
- While held, `restart_nonce`/`target` requests are deferred (the nonce is watermarked so a bump made
  during the hold cannot fire a stale restart on release). The keep-alive failure pause (`status =
  error`) is independent; a hold takes precedence over respawning.

## [0.0.7] - 2026-06-20

### Added

- **Config-change notification (design §7): a `lode.toml` edit while the app is RUNNING no longer
  does nothing — lode now NOTIFIES the app instead of auto-restarting it.** lode watches `lode.toml`
  while the app runs and, on an edit, bumps a new lode-owned `state.json` field **`config_generation`**
  (a monotonic counter) — it does **not** restart the app (a running app is never disturbed by an edit).
  The app observes the bumped counter and, at its own pace, requests the restart by bumping
  `restart_nonce`.

### Changed (behavioral)

- **A normal (`Run`-phase) restart — via `restart_nonce` or the configured restart signal — now
  RE-READS `lode.toml`** (relaunches via the config-reload path), so an edited `[env]`/config is
  applied on the relaunch. Previously a nonce/signal restart re-spawned with the in-memory config.
  (A restart during an in-progress staged-update prepare/observation still re-spawns in place to
  preserve rollout safety; the edited config is applied on the next normal restart.)
- Net effect (the §7 contract): apply a `[env]`/config change to a running app with
  **edit `lode.toml` → app sees `config_generation` bump → app bumps `restart_nonce` → lode reloads**.
  lode never auto-restarts on a config edit. A paused app keeps the existing "edit → auto reload +
  re-attempt" recovery. Host-process env (`-e`/k8s) still requires restarting lode itself.

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
