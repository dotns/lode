# lode source adapters — signing & implementation guide

**English** · [中文](source-adapters.zh-CN.md)

lode fetches updates from exactly one source: a **native** manifest URL, or a
**GitHub Releases** repo. Both resolve to the same internal artifact and the same
signature, so verification and install never branch on the source. This document
is the normative spec for the signed message, the asset/manifest shapes, the
operator config, and the publisher signing workflow.

The operator names the exact asset to install; that filename is the selection key
in both sources. There is no platform detection and no arch-alias table.

---

## 1. The signed artifact message

The signature is made with the publisher key's algorithm (ed25519 by default —
see *Keys*) over a canonical message — UTF-8, `\n`-separated, **no trailing
newline**:

```
lode.artifact.v1
{name}
{version}
{sha256}
{run}
{exec}
```

| field | meaning | source |
|---|---|---|
| `name` | the **asset filename** (e.g. `myapp-linux-x64.tar.gz`) | the selection key; what the signature's identity binds |
| `version` | the release version | github: `tag_name` minus a leading `v`; native: the `versions` map key |
| `sha256` | lowercase hex of the **raw downloaded file** (pre-unpack) | github: asset `digest`; native: the asset's `sha256` |
| `run` | manifest-published bare-run launch override (empty string when absent) | the asset's `run` field, or `""` |
| `exec` | manifest-published passthrough launch override (empty string when absent) | the asset's `exec` field, or `""` |

`name` is the **asset filename, not the application name**. It is the only field
that binds *which* artifact a signature authorises, so it prevents replaying one
artifact's signature under another asset or version. The filename also carries the
brand and platform by convention, and its extension determines the format — none
of which need to be signed separately.

### Keys

Every key carries a signature algorithm (`alg`). Unstated = `ed25519`, so keys,
configs and manifests from before `alg` existed are unchanged.

| `alg` | public key (base64) | signature (base64) | hash |
|---|---|---|---|
| `ed25519` (default) | raw 32 bytes | 64 bytes | — |
| `ecdsa-p256` | SEC1 point; compressed 33 bytes canonical (uncompressed accepted) | 64-byte `r ‖ s` | SHA-256 |
| `ecdsa-p384` | SEC1 point; compressed 49 bytes canonical (uncompressed accepted) | 96-byte `r ‖ s` | SHA-384 |

- `key_id` = first 16 hex chars of `sha256(public_key)` over the canonical
  encoding above.
- Operators pin publishers in `[trust].trusted_keys` as
  `<alg>:<key_id>:<base64pub>`; for ed25519 the `<alg>:` prefix is omitted
  (`<key_id>:<base64pub>`, the pre-existing form). `lode-cli keygen --alg <alg>`
  prints the entry ready to paste.
- **The algorithm is bound to the trusted key, never taken from the manifest.**
  lode checks a signature with the algorithm pinned on each trusted key it tries;
  the manifest's optional `alg` field is advisory. Key bytes relabelled under
  another algorithm fail to decode, so no signature is ever verified with a
  primitive the operator did not pin for that key.
- Sign: `sig = base64(sign(private_key, message))` — the message bytes are the
  same under every algorithm.
- Verify: lode accepts the artifact iff `sig` validates against **any** trusted
  key over the reconstructed message **and** the downloaded bytes hash to
  `sha256`.

### What the signature binds — and does not

Binds: which asset (`name`), which release (`version`), which bytes (`sha256`), and which launch commands (`run`/`exec`).
Does **not** bind `platform`, `format`, or `url` — these are derived from
the filename or are operator-local (below). Because `name` is the filename and is
signed, a tampered catalog cannot move a genuine signature onto different bytes,
a different asset, a different version, or inject malicious launch commands.

## 2. Catalog (manifest-level) signature — optional, verify-if-present

The native manifest **may** carry a top-level `key_id` + `sig` over the catalog. It
is an *optional* tamper-evidence layer, **never required** and **not** gated by
`require_signature`:

- **present** → the loader verifies it (a swapped `latest`, an added/removed version
  or a rewritten asset digest is caught before any download); a present-but-invalid
  signature is rejected.
- **absent** → accepted under every policy, including `enforce`.

What enforces trust is therefore *not* the catalog signature but two source-agnostic
layers that always apply: the **per-artifact** signature (§1), which `require_signature`
does gate and which binds every downloaded file; and the **client-side downgrade
floor** (§2a), which protects the channel `latest` pointer against rollback. This is
why a GitHub release (no catalog signature, §5) and a native catalog that signs only
its artifacts both work under `enforce`.

Canonical message (when a catalog *is* signed):

```
lode.manifest.v1
{name}
{key_id}
{canonical}        # deterministic, sig-free serialization of channels + versions
```

`canonical` lists, in sorted order, each `channel\t{name}\t{latest}` and per
version each `asset\t{name}\t{sha256}`. GitHub has no catalog signature — its
freshness comes from tag authority (§5).

### 2a. Client-side downgrade floor (`latest` rollback protection)

Because the catalog signature is optional, the defence for the channel `latest`
pointer lives on the client, not in the catalog: when *following* `latest` (the
default, or an explicit `update --version latest`), the loader refuses to resolve a
version **older than the floor** — the highest version it has already committed to
(`max(current, last_good)` from `state.json`). A tampered *or replayed* catalog that
points `latest` back at an older — even legitimately-signed — version is rejected
before any download.

Only *pointer-following* resolution is guarded. A deliberate downgrade is always
allowed: an explicit `update --version X`, a configured `[update].pin`, or
`lode rollback`. Comparison is by semver precedence; a non-semver `latest`/floor
can't be ordered, so a downgrade can't be proven and is allowed.

## 3. Asset naming & format

- The **filename is the selection key.** The operator sets `[update].asset` to the
  exact asset they want on this host; lode matches it against the source's asset
  list by `name`.
- **`format` is derived at runtime from the filename extension** (longest match):

  | suffix | format |
  |---|---|
  | `.tar.gz`, `.tgz` | `tar.gz` |
  | `.gz` | `gz` |
  | `.zip` | `zip` |
  | (anything else / none) | `raw` |

  The extension is authoritative — name assets so the suffix reflects the real
  packaging.

## 4. Launch overrides (`run`/`exec`) and format inference

The `entry` concept is gone. **`format`** is inferred at runtime from the asset filename's extension (§3) and is never stored or signed. A manifest asset may carry optional **`run`** and **`exec`** string fields that override the operator's `[command].run`/`exec` launch commands. These fields are signature-bound in both the per-artifact signed message (§1) and the catalog signature (§2), so a tampered catalog cannot inject malicious launch commands under `require_signature = auto` (with keys) or `enforce`.

## 5. Source adapter — GitHub Releases

```toml
[update]
github = "owner/repo"
asset  = "myapp-linux-x64.tar.gz"
```

| internal field | from the GitHub API |
|---|---|
| `name` | asset `name` (matched against `asset`) |
| `version` | release `tag_name` (drop a leading `v` before a digit) |
| `sha256` | asset `digest` (strip the `sha256:` prefix), re-verified against the downloaded bytes |
| `sig` | asset **`label`** (the only arbitrary-string slot the API returns) |
| `url` (runtime) | `browser_download_url` |

- **Version pointer = tag authority.** `channel = stable` → `/releases/latest`;
  any other channel → newest non-draft prerelease; `pin` → `/releases/tags/{tag}`.
- `browser_download_url` 302-redirects to a CDN host; this is transparent —
  verification uses the recorded fields, never the redirect target.

### Publishing — GitHub Actions release workflow

A tag push runs the release job. **Signing is optional**: the job signs only when a
signing key is configured (the `LODE_SIGNING_KEY` secret is non-empty), and falls
back to uploading unsigned assets otherwise — so forks and key-less repos still cut
releases. Steps:

1. **Build** the assets for each target into `dist/` using the agreed naming
   (`lode-<os>-<arch>.tar.gz`).
2. **Create** the release for the tag.
3. **For each asset, sign-if-keyed then upload:** if `LODE_SIGNING_KEY` is set, sign
   it and upload with the signature as the asset `label` (`file#label`); otherwise
   upload the bare file and warn that it is unsigned.

```yaml
# .github/workflows/release.yml
on:
  push:
    tags: ['v*']
permissions:
  contents: write                       # create the release + upload assets
jobs:
  release:
    runs-on: ubuntu-latest
    env:
      GH_TOKEN: ${{ github.token }}
      LODE_SIGNING_KEY: ${{ secrets.LODE_SIGNING_KEY }}   # optional — empty in forks / when unset
    steps:
      - uses: actions/checkout@v4
      - name: Build release assets        # -> dist/lode-<os>-<arch>.tar.gz  (+ the lode-cli binary)
        run: ./scripts/build-release.sh "$GITHUB_REF_NAME"
      - name: Create release
        run: gh release create "$GITHUB_REF_NAME" --generate-notes --verify-tag
      - name: Sign (only if a key is configured) and upload
        run: |
          set -euo pipefail
          TAG="$GITHUB_REF_NAME"
          for f in dist/lode-*.tar.gz; do
            if [ -n "${LODE_SIGNING_KEY:-}" ]; then
              sig=$(lode-cli sign "$f" --version "$TAG" --key-env LODE_SIGNING_KEY)
              gh release upload "$TAG" "$f#$sig"      # label = signature
            else
              gh release upload "$TAG" "$f"           # unsigned
              echo "::warning::LODE_SIGNING_KEY not set — $(basename "$f") uploaded UNSIGNED"
            fi
          done
```

Notes:

- **Key-existence gate.** A secret cannot be used in a step `if:`, so it is mapped to
  `env` and tested with `[ -n "${LODE_SIGNING_KEY:-}" ]`. In forks and unconfigured
  repos the secret is empty → the job uploads unsigned and never fails for lack of a
  key.
- **`--key-env`** reads the base64 key seed from the named env var so the private key
  never touches disk in CI. The key must live in a protected repo/org secret (or be
  signed out-of-band offline for the strongest custody).
- **`lode-cli`** is the multi-call binary built in step 1; sign with the freshly built
  one (other projects install `lode-cli` first).
- **Unsigned consequences.** An asset with no `label` is unsigned: consumers must run
  `require_signature = off` (or `auto` with no trusted keys → installs **UNVERIFIED**
  with a warning). Under `require_signature = enforce` an unsigned asset is rejected.

## 6. Source adapter — native manifest

```toml
[update]
manifest = "https://releases.example.com/myapp/manifest.json"
asset    = "myapp-linux-x64.tar.gz"
```

The manifest is an operator-hosted JSON shaped like a self-hosted release listing.
Schema `lode/v1`; per-version `assets[]` keyed by `name`:

```json
{
  "schema": "lode/v1",
  "name": "myapp",
  "key_id": "<key_id>",
  "channels": { "stable": { "latest": "1.5.0" } },
  "versions": {
    "1.5.0": {
      "notes": "…",
      "assets": [
        { "name": "myapp-linux-x64.tar.gz",
          "url": "https://.../myapp-linux-x64.tar.gz",
          "sha256": "…", "sig": "…",
          "run": "./myapp", "exec": "./myapp", "size": 5242880 },
        { "name": "myapp-darwin-arm64.tar.gz",
          "url": "https://.../myapp-darwin-arm64.tar.gz",
          "sha256": "…", "sig": "…" }
      ]
    }
  },
  "sig": "<catalog signature — optional, see §2>"
}
```

| asset field | required | meaning |
|---|---|---|
| `name` | ✓ | selection key; matched against `[update].asset` |
| `url` | ✓ | absolute download URL |
| `sha256` | ✓ | lowercase hex of the raw file |
| `sig` | enforce / auto+keys | base64 signature (the signing key's algorithm, §1 *Keys*) over the §1 message (including `run`/`exec`); inline, or supply a `.sig` sidecar alongside the asset |
| `alg` | | advisory: the signing key's algorithm (`ed25519` when absent); verification always uses the trusted key's own algorithm |
| `run` | | optional literal launch command override (signature-bound; overrides `[command].run`; see §4) |
| `exec` | | optional CLI-passthrough command override (signature-bound; overrides `[command].exec`; see §4) |
| `size` | | expected byte count (extra integrity check) |
| `auth` | | default `true`; `false` = don't attach `[http].headers` to this URL |

- **Version pointer.** Rollback of `channels.<c>.latest` is caught client-side by
  the downgrade floor (§2a) — no catalog signature is required for it. Signing the
  catalog (§2) is still recommended as up-front tamper-evidence; a `pin` removes all
  trust in the pointer entirely.
- Native may carry more than GitHub (`channels`, `notes`, detached
  `.sig`, `size`, `auth`); all of it still reduces to `(name, version, sha256) +
  sig` at the bottom.

**Publishing:**

```bash
lode-cli manifest "$f" --version 1.5.0 --url "$URL" \
    --run ./myapp --exec ./myapp \
    --key private.key --into manifest.json     # upserts the asset by name, sets channels.latest; --run/--exec are optional
lode-cli manifest-sign --into manifest.json --key private.key   # optional §2 catalog tamper-evidence
```

Host `manifest.json` + the assets at any HTTPS URLs.

## 7. Operator config (`lode.toml`)

```toml
[update]
github   = "owner/repo"           # OR  manifest = "https://.../manifest.json"  (pick one)
asset    = "myapp-linux-x64.tar.gz"   # the asset filename for THIS host (the selection key)
channel  = "stable"
policy   = "auto"                 # off | check | auto
# pin    = "1.4.2"                # lock a version (disables auto-update)

[trust]
require_signature = "enforce"     # off | auto | enforce — gates the PER-ARTIFACT
                                  #   signature (§1). off: integrity only. auto:
                                  #   required once a trusted key is configured.
                                  #   enforce: always required. The catalog signature
                                  #   (§2) is verify-if-present and never gated here.
trusted_keys = ["<key_id>:<base64-pubkey>"]
```

## 8. Component responsibilities (implementation map)

| module | responsibility |
|---|---|
| `verify.rs` | the §1 artifact message (`lode.artifact.v1`) and §2 catalog message (`lode.manifest.v1`); `verify_artifact_sig` over `(name, version, sha256, run, exec)` |
| `manifest.rs` | internal `Manifest` with per-version `assets[]` keyed by `name`; select the asset by `name`; derive `format` from the extension; both adapters (`fetch_github`, `fetch_native`) produce the identical internal model |
| `config.rs` | `[update].asset`; `manifest`/`github` stay mutually exclusive |
| `download.rs` | fetch by `url`; attach `[http].headers` only same-origin; cross-check the GitHub `digest` and re-hash the downloaded file against the signed `sha256` |
| `authoring.rs` / `lode-cli` | `keygen`; `sign` → the `(name, version, sha256)` signature and the GitHub `label` string; native `manifest` assembly + `manifest-sign` over the §2 catalog form |

Downstream (`resolve_target`, install, supervise) is shared and source-agnostic.
