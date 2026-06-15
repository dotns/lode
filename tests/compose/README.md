# tests/compose — lode docker-compose end-to-end integration

A REAL, fully-local docker proof that the distroless `lode` image loads packaged
apps, auto-updates, rolls back from a bad release, updates by app-exit, and bounds
an opt-in restart loop — with signature verification ENFORCED and **no real network
/ GitHub**. Driven entirely by `tests/src/integration/compose.test.ts` (bun + TS;
the test is docker-gated and self-skips when docker is absent).

## Services (`docker-compose.yml`)

| service | image | role |
|---|---|---|
| `fileserver` | `lode-fileserver:e2e-<suffix>` (built from `fileserver/Dockerfile` + the static `lodetest`) | writable lode/v1 manifest + artifact server, fixed per-run IP |
| `svc-rust` | `lode:e2e-<suffix>` (repo `Dockerfile`) | native binary app (`tests/apps/web-rust`), no `[runtime]`; `policy=auto`, `readiness=state` |
| `svc-bun` | `lode:e2e-<suffix>` | script app (`tests/apps/web-bun`) under a stubbed `bun` `[runtime]` (the static `lodetest`) |
| `svc-restart` | `lode:e2e-<suffix>` | crashing app with `restart="always"` + `restart_max=3` → bounded restarts then exit |

Per-service config lives in `svc-*/lode.toml`. The publisher's trusted key is
generated per test run (`lode keygen`) and injected via `LODE_TRUSTED_KEYS`.

**Concurrency isolation:** every daemon-global identifier — compose project,
network name, subnet, fileserver IP, image tags — is derived by the test from a
hash of the worktree path (`<suffix>`), so concurrent checkouts can run the suite
against the same docker daemon without tearing down each other's stacks. The
values reach `docker-compose.yml` via `LODE_E2E_*` env interpolation (whose
`:-` defaults keep the file usable standalone), and the default fileserver IP in
`svc-*/lode.toml` is rewritten to the per-run address before `docker cp`.

## What the test proves

1. `docker build` of the repo `Dockerfile` produces a working **distroless static** image.
2. Both apps install + serve **v0.0.1**, then **auto-update v0.0.1 → v0.0.2**.
3. A **crashing v0.0.3** is **single-strike rolled back to v0.0.2** (both apps).
4. **Update-by-app-exit**: `svc-bun`'s app writes `state.target` then `exit(0)`, and
   lode relaunches DIRECTLY on the new version (no flap to the old one).
5. **Opt-in `restart=always`** bounds the crash loop at `restart_max` then exits
   `status=error` (vs. the `restart=off` mirror default the other services use).
6. `docker compose down -v` tears everything down cleanly.

## Why `docker cp` / `docker exec` instead of bind-mounts + published ports

So the test passes both on a normal docker host (CI) **and** in
docker-out-of-docker sandboxes, where the test process shares the daemon socket but
NOT its network/mount namespaces (there, host bind-mounts share the wrong files and
published ports are unreachable). The fixed fileserver IP lets the static
distroless binaries reach it without DNS/NSS; the fileserver carries a tiny
`lodetest get` HTTP client so the test can probe the apps container-to-container.

## The `lodetest` helper

`lodetest/` is a std-only static binary (its own cargo workspace, like
`tests/apps/web-rust`, so the repo cargo gate never builds it). Modes:

- `lodetest serve <root> [port]` — read-fresh HTTP file/manifest server (fileserver).
- `lodetest get <url>` — minimal HTTP GET client (IP host, no DNS) for probing.
- `lodetest <script.ts>` — a stand-in `bun` runtime that runs the web-bun app
  contract (version, readiness, graceful stop, bad-mode, update-by-app-exit) so the
  bun service works in distroless/static where real `bun` (needs glibc) cannot.

## Run it

```sh
cargo build --bins                 # debug lode for signing (LODE_BIN)
cd tests && bun install --frozen-lockfile
LODE_BIN=../target/debug/lode bun test --timeout 120000 src/integration/compose.test.ts
```
