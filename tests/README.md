# tests

All of lode's test and example material lives here.

```
tests/
├── src/                     bun + TypeScript end-to-end suite (ci.yml: `bun test src/`)
│   ├── 01-…29-*.test.ts     scenario tests (bootstrap, update, signature, restart matrix, rollback, hold, runtime cache, signals, GitHub source, …)
│   ├── helpers/             harness, lode driver, manifest server, signer, app builder
│   ├── fixtures/app.sh      the versioned POSIX-sh app artifact under test
│   └── integration/         docker-compose integration test (docker-gated; self-skips without docker)
├── apps/                    example apps (also used as test fixtures)
│   ├── web-rust/            minimal std-only Rust HTTP server (native, no runtime)
│   └── web-bun/             Bun/TypeScript HTTP server (runs under a `[runtime]`)
└── compose/                 docker-compose integration stack (two lode services + fixtures)
```

## Run the e2e suite

```bash
cargo build --bins                                       # build the lode binary
cd tests && bun install
LODE_BIN=../target/debug/lode bun test src/              # all scenarios
```

The `src/integration/compose.test.ts` integration test additionally needs Docker; it
self-skips when Docker is unavailable, so the rest of the suite still runs everywhere.

## Example apps

`apps/web-rust` and `apps/web-bun` both implement the same language-agnostic **lode app
contract** (`GET /version`, `GET /healthz`, graceful `SIGTERM` stop, `state.ready`
readiness handshake, an optional crash-on-startup “bad” mode for rollback testing). They
double as the reference for packaging your own app — see each app's `README.md` and the
[app integration guide](../docs/integration.md).

## Coverage notes

The scenario suite drives the real binary through every design flow it can reach
from a single host without external services: both sources (native manifest;
GitHub Releases via the local API stand-in in `src/helpers/githubServer.ts`),
signature enforcement, the update / rollback / restart matrix, readiness and
staged-update handshakes, hold, config reload, the download and runtime caches,
and signal passthrough. The following stay **unit-level only** (in `crates/`), by
choice — each is a small, pure function of its inputs and has no cross-process
behaviour the e2e layer would add:

| feature | where it is tested |
|---|---|
| `[trust].trusted_keys_file` parsing (comments, blanks, entry forms) | `lode-core/src/verify.rs`, `install.rs` |
| `keep_versions` pruning | `lode-core/src/install.rs` (`prune`) |
| native `.sig` sidecar fallback | `lode-core/src/manifest.rs` |
| `[http].credential_hosts` / `allow_insecure` against a remote host | `lode-core/src/download.rs`, `http.rs` |
| `[command].workdir` expansion | `lode-supervisor/src/lib.rs` |
