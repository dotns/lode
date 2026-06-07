# Integrating an app with lode

**English** · [中文](integration.zh-CN.md)

Integration spans three files, each with one owner:

| File | Where | Who writes it | Purpose |
|---|---|---|---|
| **`lode.toml`** | local | the operator | how lode fetches & runs your app |
| **`state.json`** | local (`$DATA_DIR`) | lode **and** the app | runtime comms (status ↔ requests) |
| **release feed** | remote | the publisher | the signed asset listing — a native `manifest.json` **or** GitHub Releases |

The three steps below — **configure → run → publish** — are the whole integration. The
operator names the exact asset to install (`[update].asset`); that filename is the
selection key for both sources. For the full signing spec see
[source-adapters.md](source-adapters.md); for exhaustive field listings see
[`lode.example.toml`](lode.example.toml) and [`manifest.example.json`](manifest.example.json);
the deep design is in [architecture](architecture.md).

---

## 1. Configure lode (`lode.toml`)

The operator's file: *how to fetch and run your app*. The app never writes it.
Precedence is `CLI > env (LODE_*) > lode.toml > defaults`; by default lode reads
`/srv/lode/lode.toml` (override the base dir with `LODE_DATA_DIR`) and scaffolds a
starter there on first run.

```toml
[global]
app      = "myapp"          # must match the manifest "name"
data_dir = "/srv/lode"      # holds lode.toml + versions/ + state.json + lode.pid + runtime/

[update]
github   = "owner/myapp"                                        # GitHub Releases …
# manifest = "https://releases.example.com/myapp/manifest.json" # … OR a native manifest (pick one)
asset    = "myapp-linux-x64.tar.gz"   # the asset filename for THIS host (the selection key)
channel  = "stable"         # github: stable=/releases/latest, else newest prerelease ; native: channel key
policy   = "auto"           # off | check | auto
# pin    = "1.4.2"          # lock a version (disables auto-update)
# entry  = "bin/myapp"      # override the in-archive entry; usually omitted (defaults to {app} at root)

[trust]
require_signature = "enforce"                       # off | auto | enforce
trusted_keys = ["<key_id>:<base64-pubkey>"]         # from `lode-cli keygen`

[command]
run     = "{entry}"         # bare `lode` → launch the app ({entry} = installed path)
exec    = "{entry}"         # `lode <args>` → passthrough base
workdir = "{dir}"           # version dir (default) or an absolute path

[supervise]
readiness    = "state"      # none | state (commit a version only after the app reports ready)
health_grace = 15           # seconds a new version must survive to be "good" (else rollback)
stop_timeout = 10           # graceful-stop window before SIGKILL
restart      = "off"        # off (mirror child) | on-failure | always
```

Common shapes:

- **Self-contained binary:** `run = "{entry} serve"`, `exec = "{entry}"`.
- **Script under a runtime:** `run = "bun run"`, `exec = "bun"`, plus a `[runtime]` block to fetch `bun` if it's not on PATH (verified like any artifact).
- **Private source:** add `[http].headers = ["Authorization: Bearer ${TOKEN}"]` — sent on manifest + artifact fetches, with `${ENV}` expansion.

See [`lode.example.toml`](lode.example.toml) for every option and `[runtime]`/`[signals]`/`restart_*`.

---

## 2. The app contract (`state.json`)

What *your app* implements. Any language — read/write one JSON file and handle `SIGTERM`.

**Environment lode injects:** `LODE_ACTIVE_VERSION` (current version), `LODE_DATA_DIR`
(`state.json` lives at `$LODE_DATA_DIR/state.json`), `LODE_INSTANCE` (unique id for this
launch — write it to `state.ready`). The host env (e.g. `PORT`) passes through; internal
`LODE_*` are stripped.

**state.json** — lode writes status, the app writes requests; the field sets don't overlap:

```jsonc
{
  // lode writes (app reads):
  "current": "1.4.2", "last_good": "1.4.2", "available": "1.5.0",
  "status": "running",        // starting|running|updating|rolling-back|stopping|stopped|error
  "pid": 12345, "last_check": "…", "last_error": null,
  // app writes (requests / readiness):
  "target": null,             // a version or "latest" => request up/down-grade
  "restart_nonce": 0,          // increment => restart the current version
  "ready": null               // set to LODE_INSTANCE => "I can serve now"
}
```

Implement these (all but `SIGTERM` are optional, but recommended):

- **Graceful stop (required):** on `SIGTERM`, drain and `exit(0)` within `stop_timeout`, or you're `SIGKILL`ed. lode sets `status = updating|stopping` first so you can distinguish.
  ```ts
  process.on("SIGTERM", async () => { await drain(); process.exit(0) })
  ```
- **Readiness (if `readiness = "state"`):** after you can actually serve, atomically write `state.ready = LODE_INSTANCE` (temp-file + rename, preserving lode's fields). lode won't commit the version (or stop the old instance, in zero-downtime modes) until then; missing it past `ready_timeout` → rollback.
- **Health:** `exit(non-zero)` on startup failure. A version that exits within `health_grace` is rolled back to the last good one (single-strike).
- **Self-report version** (e.g. `GET /version`) matching `LODE_ACTIVE_VERSION`.
- **Request an update/restart (optional):** atomically patch `state.json` — set `target` (a version or `"latest"`) or bump `restart_nonce`. lode polls the file's mtime (~1s) and acts; the file *is* the notification.

> A worked Rust + Bun pair lives in [`../tests/apps`](../tests/apps).

---

## 3. Publish the release feed

lode resolves a **channel → version → asset**, verifies it, and installs/runs it.
The asset each host installs is chosen by **filename** (`[update].asset`), and every
asset carries an ed25519 signature over the canonical message
`lode.artifact.v1\n{name}\n{version}\n{sha256}` (UTF-8, `\n`-separated, no trailing
newline). `name` is the asset filename. Full spec, including the native manifest
shape and field tables: [source-adapters.md](source-adapters.md).

Packaging + signing are the **publisher's** job, doable in any CI. `lode-cli` is a
reference implementation; any ed25519 tooling that produces the same signature works.

### Keys (once)

`lode-cli keygen` prints `key_id`, the `trusted_keys` entry (`<key_id>:<base64>`,
hand to operators), and the secret seed — keep it offline.

### GitHub Releases (`github = "owner/repo"`)

Drop this workflow into **your app's** repo. It builds your assets and — **only if a
signing key is configured** — signs each one and uploads the signature as the asset
`label`. With no key it uploads unsigned, so it works before you adopt signing.

```yaml
# .github/workflows/release.yml — publish your app's assets for lode
on:
  release:
    types: [published]      # cut the release (UI or `gh release create`); this attaches assets
permissions:
  contents: write
jobs:
  release:
    runs-on: ubuntu-latest
    env:
      GH_TOKEN: ${{ github.token }}
      TAG: ${{ github.event.release.tag_name }}
      LODE_SIGNING_KEY: ${{ secrets.LODE_SIGNING_KEY }}   # optional — set it to enable signing
    steps:
      - uses: actions/checkout@v4

      - name: Build assets                # -> dist/<app>-<os>-<arch>.<ext>   (you supply this)
        run: ./build.sh "$TAG"

      - name: Publish (sign only if a key is set)
        run: |
          set -euo pipefail
          if [ -n "${LODE_SIGNING_KEY:-}" ]; then
            curl -fsSL https://github.com/dotns/lode/releases/latest/download/lode-linux-x64.tar.gz \
              | tar -xz lode lode-cli                 # fetch lode-cli to sign with
          fi
          for f in dist/*; do
            if [ -n "${LODE_SIGNING_KEY:-}" ]; then
              sig=$(./lode-cli sign "$f" --version "$TAG" --key-env LODE_SIGNING_KEY)
              gh release upload "$TAG" "$f#$sig" --clobber     # label = signature
            else
              gh release upload "$TAG" "$f" --clobber          # unsigned
            fi
          done
```

- **Enable signing:** run `lode-cli keygen` once; put the secret seed in the repo's
  `LODE_SIGNING_KEY` secret (keep a copy offline), and give operators the public
  `trusted_keys` entry. No secret set → assets upload unsigned (fine until you adopt it;
  the sign branch never runs).
- lode picks the asset whose `name` equals the operator's `[update].asset`; `sha256`
  comes from the asset `digest` (re-verified against the bytes), `version` from the tag.
  `channel = stable` → `/releases/latest`; other channels → newest non-draft prerelease;
  `pin` → a specific tag. No `manifest.json` asset is needed. Private repo: token in
  `[http].headers`.
- Name assets `<app>-<os>-<arch>.<ext>`; each operator sets `[update].asset` to the exact
  filename for their host.

### Native manifest (`manifest = "https://.../manifest.json"`)

Host a `lode/v1` manifest whose per-version `assets[]` are keyed by `name`, plus the
assets at any HTTPS URLs:

```bash
lode-cli manifest "$f" --version 1.5.0 --url "$URL" --entry bin/myapp \
    --key private.key --into manifest.json   # upserts the asset by name, sets channels.latest
lode-cli manifest-sign --into manifest.json --key private.key   # signs the catalog
```

Manifest shape + the per-asset field table live in
[source-adapters.md §6](source-adapters.md). `channels.<c>.latest` must be signed
(`manifest-sign`) or the operator must `pin` a version.

### Signing model (both sources)

- The artifact signature binds **`name` (filename) / `version` / `sha256`** only.
  `format` is derived from the filename extension (`.tar.gz`/`.tgz` → tar.gz, `.gz`
  → gz, `.zip` → zip, else raw). `entry` is **advisory** and **never signed**
  (resolution: manifest hint > `lode.toml [update].entry` > `{app}` at archive
  root).
- Under `require_signature = enforce`, every installed asset must carry a valid
  signature (github: the `label`; native: the `sig` field or a `.sig` sidecar).
  `auto` is fail-closed once any trusted key is configured; without keys it installs
  **UNVERIFIED** with a warning.

### Checklist

- [ ] each host's `[update].asset` names the exact asset filename for its platform.
- [ ] `sha256` is of the raw file; `sig` is over `name/version/sha256` with a trusted `key_id`.
- [ ] github: signature set as the asset **`label`**. native: `sig` inline or a `.sig` sidecar, and the catalog re-signed (`manifest-sign`) after the final edit.
- [ ] `channels.<c>.latest` points at a real version (native), or tag/latest resolves (github).
- [ ] private key offline; operators hold only the public `trusted_keys` with `require_signature = enforce`.
