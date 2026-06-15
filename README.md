# lode

**English** · [中文](README.zh-CN.md)

> A universal **“verify · launch · update”** loader: one small static Rust binary that
> verifies a packaged app (integrity **and** publisher identity), launches it, supervises
> it, and hot-updates it. Bake it into a generic image once — switching apps is just a
> different manifest, never an image rebuild.

- **Image:** `docker.io/dotns/lode` ([Docker Hub](https://hub.docker.com/r/dotns/lode))
- **Binaries:** Linux (x86_64 / aarch64, musl-static) + macOS (x86_64 / arm64) — [Releases](https://github.com/dotns/lode/releases)
- **Platforms:** Unix only (lode is a process supervisor — PID-1 subreaper, signal forwarding, `exec` passthrough).

## Start here — by role

| You are… | You want to… | Go to |
|---|---|---|
| **Operator** | run & keep an app updated in a container | [Quick start](#quick-start) + [`lode.example.toml`](docs/lode.example.toml) |
| **App author** | make your app updatable by lode | [Integration §2 — the app contract](docs/integration.md) |
| **Publisher** | package, sign & publish a release | [Integration §3 — publish versions](docs/integration.md) |
| **Curious** | understand the design | [Architecture](docs/architecture.md) |

The [Integration guide](docs/integration.md) covers the whole chain — configure (`lode.toml`) → run (`state.json`) → publish (`manifest.json`).

Full doc index (bilingual): [`docs/`](docs/README.md). Working examples:
[`tests/apps`](tests/apps) (a Rust + a Bun server) and [`tests/compose`](tests/compose) (live update/rollback).

## Quick start

Point lode at a signed manifest and run the generic image. By default lode reads
`/srv/lode/lode.toml` and keeps its state under `/srv/lode`:

```bash
docker run --rm \
  -v "$PWD/lode.toml:/srv/lode/lode.toml:ro" \
  -e LODE_TRUSTED_KEYS="<key_id>:<base64-pubkey>" \
  docker.io/dotns/lode:latest
```

A minimal `lode.toml` (see [`docs/lode.example.toml`](docs/lode.example.toml) for all options):

```toml
[global]
app = "myapp"
[update]
manifest = "https://releases.example.com/myapp/manifest.json"   # or: github = "owner/repo"
policy   = "auto"                                               # off | check | auto
[command]
run = "./myapp"                                                # how to launch the app (literal command, cwd = version dir)
[trust]
require_signature = "enforce"
```

> If `/srv/lode/lode.toml` is missing on first run, lode scaffolds a starter there and tells
> you to fill in the source. Override the base dir with `LODE_DATA_DIR`. No config file needed
> if you pass `--manifest`/`--github` (or `LODE_*`) instead.

To build your own app image, layer lode onto any base:

```dockerfile
FROM oven/bun:1                       # or any runtime your app needs
COPY --from=docker.io/dotns/lode:latest /usr/bin/lode /usr/bin/lode
ENTRYPOINT ["/usr/bin/lode"]
```

## How it works

```
generic image           ┌─────────────────────────────────────┐
zzci/ubase         ────► │  lode  (static Rust binary)         │
                         └───────────────────┬─────────────────┘
                                             │ lode.toml + env + CLI
                                             ▼
   [update].manifest ──HTTPS(+headers)──► manifest.json  (channels → versions → assets[name])
                                             │  (remote; never stored locally)
                            pick platform ──┤── download → verify sha256 + ed25519
                                             ▼
                    $DATA_DIR/versions/<ver>  ──(atomic rename)──► current
                                             │
                                             ▼
              lode            → runs `run`  (supervised service: auto-update + rollback)
              lode <args…>    → runs `exec` + <args>  (one-shot CLI passthrough)
```

## Two binaries, one file

lode is a **multi-call binary**. As `lode` it is the loader with **no subcommands** — arguments
are forwarded to the app. One caveat: lode parses its own flags first (`--version`, `--help` and
the `LODE_*` global options), so a leading app argument that matches one of them is consumed by
lode. Use `lode -- <args…>` to forward flag-like arguments verbatim. As **`lode-cli`** (a symlink
shipped alongside it) it is the operator/publisher toolkit.

| Invocation | Does |
|---|---|
| `lode` | start & supervise the app (`[command].run`); auto-update per policy |
| `lode <args…>` / `lode -- <args…>` | passthrough: run `[command].exec` + `<args>` (e.g. `lode run db:init`); use `--` when an arg collides with a lode flag |
| `lode-cli status` / `update` / `rollback` / `restart` / `versions` | manage a running instance (via `state.json`) |
| `lode-cli keygen` / `sign` / `verify` / `manifest` / `init` | publisher/operator tools |

## Three files

- **`lode.toml`** — local TOML; the operator's config (how to fetch & run). The app never writes it. → [`docs/lode.example.toml`](docs/lode.example.toml)
- **`state.json`** — local JSON; runtime comms. lode writes status; the app writes requests (`target`/`restart_nonce`/`ready`). → [Integration §2](docs/integration.md)
- **`manifest.json`** — remote JSON; the signed version catalog (never stored locally). → [`docs/manifest.example.json`](docs/manifest.example.json)

## Key behaviors

- **Update** `[update].policy = off | check | auto`; source is either `manifest` (native `lode/v1` JSON) **or** `github = "owner/repo"` (Releases).
- **Rollback** — a new version that exits within `health_grace` is reverted to the last known-good (single-strike).
- **Restart** `[supervise].restart = off | on-failure | always` — `on-failure` (default, keep-alive) retries a failing app `restart_max` times then **pauses** (lode stays alive, never crash-looping the container); `off` opts back into mirroring the child; lode-initiated update/rollback/restart always relaunch.
- **Trust** — `sha256` + `ed25519`; set `[trust].trusted_keys` + `require_signature = off | auto | enforce`. Note: verification defaults to `auto` (enforced only when trusted keys are configured) — set `require_signature = "enforce"` for production. Signing is the publisher's job — see [Integration §3](docs/integration.md).
- **Private sources** — `[http].headers` (with `${ENV}` expansion) is sent on every fetch.

## Build from source

```bash
cargo build --profile dist --target x86_64-unknown-linux-musl    # release static binary
cargo fmt --check && cargo clippy --all-targets && cargo test    # gates
cd tests && bun install && LODE_BIN=../target/debug/lode bun test src/   # e2e
```

Stack follows **pma-rust** (edition 2024, `#![forbid(unsafe_code)]`, deny-warnings, rustls + aws-lc-rs, musl + `+crt-static`).

## License

MIT
