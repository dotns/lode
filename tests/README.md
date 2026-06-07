# tests

All of lode's test and example material lives here.

```
tests/
├── src/                     bun + TypeScript end-to-end suite (ci.yml: `bun test src/`)
│   ├── 01-…13-*.test.ts     scenario tests (bootstrap, update, signature, restart matrix, rollback, …)
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
