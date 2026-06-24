# Local testing without a release source (`lode-cli seed`)

**English** · [中文](dev-local-testing.zh-CN.md)

For development you usually want to run a binary under lode **without** standing up a
manifest/GitHub source, signing keys, or any network. lode is built for this: it only
contacts a remote when it has **nothing usable installed**. Any version already present
on disk is launched straight from its `.lode.json` marker (design §15) — no fetch, no
signature, no download.

`lode-cli seed` installs a local binary as a version directly, so you can iterate.

## TL;DR

```bash
# install ./myapp as a version into a throwaway data dir, and activate it
lode-cli --dir /tmp/lode-dev seed ./myapp --version 1.0.0

# run it — bare `lode`, fully offline, no source configured
lode --dir /tmp/lode-dev
```

`seed` activates the version by default and, on a fresh dir, scaffolds a sourceless
`lode.toml` (so bare `lode` starts with no source). That's it.

> **`lode` vs `lode-cli`.** Bare `lode` **is** the supervised service — there is no
> `serve` subcommand; the loader's argv is reserved so `lode <args>` can transparently
> exec-proxy into the app. Management/dev subcommands (`seed`, `versions`, `status`,
> `rollback`, `restart`, `update`) live under the **`lode-cli`** name, a symlink to the
> same binary: `ln -s lode target/debug/lode-cli`.

## `lode-cli seed`

```
lode-cli [--dir DIR] [--app NAME] seed <APP_BIN> [options]

  <APP_BIN>        a local executable, or a .tar.gz / .zip / .gz archive
  --version VER    version id (default: 0.0.0-dev); keys versions/<VER>. Use semver so
                   rollback / the downgrade floor order correctly
  --no-activate    install into versions/ but don't flip current / write state.json
```

It performs the same staging + atomic activation as a real install, **minus** the
download, the sha256 integrity check and the signature check — you are placing trusted
bytes yourself. The source file is copied, not consumed.

### From a local archive (rebuild the version dir from a release `.tar.gz`/`.zip`)

Hand it the same archive a release would ship and it reconstructs the full version
directory tree under `versions/<VER>/`. `format` is derived from the extension, and the
scaffolded `lode.toml` `[command].run` is derived from the filename (an archive →
`./<app>`). The seed does **not** probe the archive for "the" binary — to launch a
binary nested at e.g. `bin/myapp`, set `[command].run = "./bin/myapp"` in `lode.toml`
(or have the manifest publish `run`):

```bash
# myapp-1.0.0.tar.gz contains  bin/myapp  (+ other files)
lode-cli --dir /tmp/lode-dev --app myapp seed ./myapp-1.0.0.tar.gz \
    --version 1.0.0
```

→ rebuilds, byte-for-byte like a real install:

```
versions/1.0.0/
├── bin/myapp           # unpacked + made executable
├── lib/…               # the rest of the archive tree, preserved
└── .lode.json          # { "version": "1.0.0", "run": null, "exec": null, "format": "tar.gz" }
```

## What it writes

```
<data-dir>/
├── lode.toml                      # scaffolded sourceless config (policy=off) if absent
├── versions/
│   └── 1.0.0/
│       ├── myapp                  # your binary, +x
│       └── .lode.json             # { "version": "1.0.0", "run": null, "exec": null, "format": "raw" }
├── current -> versions/1.0.0      # (unless --no-activate) relative symlink
└── state.json                     # (unless --no-activate) { "current": "1.0.0", "last_good": "1.0.0" }
```

## Why this runs offline

lode reaches the network in only two places, both avoided here:

1. **Startup bootstrap** — only when the data dir has *zero* installed versions. After
   `seed`, startup resolves the seeded version locally and launches from the marker;
   bootstrap never runs.
2. **Periodic update check** — disabled entirely when `[update].policy = "off"` (the
   scaffolded config) or a `pin` is set: the supervisor schedules no check, so it never
   fetches. (Even under `check`/`auto`, a fetch failure is best-effort — logged and
   ignored — so a broken remote never stops the running version.)

A sourceless config is valid: with neither `[update].manifest` nor `[update].github`
set, lode just logs "no update source configured" and runs what is installed.

## Multiple versions (rollback / downgrade testing)

Seed once per version against the same data dir, then drive `lode-cli`:

```bash
lode-cli --dir /tmp/lode-dev seed ./myapp-v1 --version 1.0.0
lode-cli --dir /tmp/lode-dev seed ./myapp-v2 --version 1.1.0 --no-activate

lode-cli --dir /tmp/lode-dev versions          # both listed; * marks current
lode-cli --dir /tmp/lode-dev rollback --version 1.0.0   # deliberate downgrade, local, no fetch
```

## Notes

- **Development/testing only.** `seed` bypasses integrity and signature checks because
  nothing is downloaded. Production installs go through `lode-cli update` (or the
  supervisor's bootstrap), which verify per `[trust].require_signature`.
- To test the *real* update flow offline, point `[update].manifest` at a local
  `http://127.0.0.1` server (see the e2e harness under `tests/`) instead of seeding.
- An empty data dir on first boot is the one case that *must* fetch (there is nothing to
  run yet). For an air-gapped first boot, `seed` a version, or copy a data dir populated
  once online.
