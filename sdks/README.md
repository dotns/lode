# lode SDKs

Single-file client libraries that let an external program integrate with the
[`lode`](../README.md) supervisor — **read status** and request **upgrade /
restart / rollback / readiness** — without re-deriving the on-disk format and
locking rules.

| File | Language | Dependencies |
|---|---|---|
| [`lode.ts`](lode.ts) | TypeScript / JavaScript (Bun or Node) | none (node:fs builtins; real `flock(2)` via `bun:ffi` under Bun) |
| [`lode.go`](lode.go) | Go (Unix) | none (stdlib only) |
| [`lode.rs`](lode.rs) | Rust (Unix) | `serde` (derive) + `serde_json` |

Drop the file for your language into your project and import it. Each is a faithful
port of the same API.

## What the SDK does — and doesn't

lode and your app share **one JSON file**, `$LODE_DIR/state.json`. lode writes
status (`current`/`last_good`/`status`/…); your app writes **requests**
(`target`/`restart_nonce`) and **readiness** (`ready`). The SDK wraps that whole
contract — see [docs/integration.md §2](../docs/integration.md) for the spec and
[tests/apps](../tests/apps) for worked reference apps.

It does **not** fetch / verify / install release artifacts. That heavy machinery is
lode's. The SDK only *requests* a version by setting `target`; a running lode
(polling `state.json`) then does download → signature-verify → install → observe →
commit-or-rollback. So `requestUpdate("1.5.0")` is the same signal the
`lode update --version 1.5.0` CLI sends to a live supervisor — likewise
`reboot()` ≈ `lode restart`, `rollback()` ≈ `lode rollback`.

**Safety.** Every write is a read-modify-write that *preserves all lode-owned and
unknown fields* (forward-compatible) and lands via atomic temp+rename. RMWs take the
sibling `state.json.lock` `flock(2)` so they serialise against lode's own writes
(Go/Rust always; TS under Bun — on plain Node it degrades to the lock-free atomic
RMW, which lode also tolerates). Plain reads need no lock.

## Environment lode injects

| Var | Meaning |
|---|---|
| `LODE_DIR` | lode's own dir, holding `state.json` (presence ⇒ "supervised by lode") |
| `LODE_WORKDIR` | the app's run dir under lode (its cwd) |
| `LODE_CONFIG` | path to the `lode.toml` lode loaded (read it read-only to see lode's config) |
| `LODE_INSTANCE` | this launch's unique id `{pid}-{nanoid}` (needed for readiness) |
| `LODE_ACTIVE_VERSION` | the version lode launched |
| `LODE_READINESS` | `none` or `state` (whether the `ready` handshake is in force) |

Your **app** keeps its own directory convention (works with or without lode): set
`ROOT_DIR` (and optionally `DATA_DIR`); resolve your data dir `DATA_DIR` > `LODE_DIR`
> `ROOT_DIR` via the `dataDir()` helper. See
[docs/integration.md → Data directories & persistence](../docs/integration.md#data-directories--persistence).

## API (same shape in all three)

| Method | Effect |
|---|---|
| `read()` | parse `state.json` → typed `State` (or null/None when absent) |
| `update(patch)` | locked RMW primitive over the raw object (preserves unknown keys) |
| `reboot()` | bump `restart_nonce` → ask lode to restart **your own** process (graceful stop + respawn) — self-recycle on a leak, periodic restart, etc. |
| `reloadConfig()` | apply a pending `lode.toml` edit — alias of `reboot()`; the restart re-reads `lode.toml` |
| `requestUpdate(version)` | set `target` (a version or `"latest"`) → up/down-grade |
| `rollback(version?)` | set `target` to `version`, else recorded `last_good` |
| `hold()` / `release()` | set/clear the `hold` flag — ask lode NOT to (re)start your process (planned maintenance); lode reports `status="held"` and waits |
| `markReady()` | `ready = LODE_INSTANCE` — "I can serve now" (bare token) |
| `markServing()` / `ackPrepared()` | phased handshake: report serving `-0` / ack cut-over `-2` |
| `prepareRequested(state?)` | is lode prompting *this* instance to prepare? (`ready == "{instance}-1"`) |
| `watch(...)` | poll loop with a change callback for every notification (config / version / status / available / error / prepare) |

Plus free helpers: `isSupervised()`, `activeVersion()`, `instanceId()`,
`readiness()`, the directory helpers `dataDir()` (resolves `DATA_DIR` > `LODE_DIR` >
`ROOT_DIR`) / `rootDir()` / `lodeDir()` / `workdir()`, read-only config access
`configPath()` / `readConfig()` (raw `lode.toml` text), and a graceful-stop handler
(`onTerminate` in TS/Go; `install_term_handler()` + `terminating()` in Rust).

## The lode ↔ app channel

`state.json` is bidirectional. The SDK covers **both** directions:

**lode → app (notifications — `read()` for the full snapshot, or subscribe to
change events via `watch`). Every mutable field has a change callback:**

| Signal | Field | `watch` callback |
|---|---|---|
| operator edited lode.toml (**no auto-restart**) | `config_generation` | `onConfigChange` |
| newer version advertised (`policy = check`) | `available` | `onAvailable` |
| lifecycle status changed | `status` | `onStatus` |
| update committed / rollback landed | `current` / `last_good` | `onVersionChange` |
| a maintenance hold was set/cleared | `hold` / `status == "held"` | `onHold` |
| lode recorded a (non-fatal) error | `last_error` | `onError` |
| staged-update prepare prompt | `ready == "{instance}-1"` | `onPrepare` (· `prepareRequested()`) |
| anything else, every tick | full `State` | `onState` |

**app → lode (requests — written under the `state.json.lock` flock):**

| Intent | Field | SDK |
|---|---|---|
| restart my own process (self-recycle / on a leak) | `restart_nonce` | `reboot()` |
| apply a pending config edit | `restart_nonce` | `reloadConfig()` |
| upgrade / downgrade | `target` | `requestUpdate(v)` |
| roll back to last good | `target` | `rollback()` |
| hold off a (re)start for maintenance | `hold` | `hold()` / `release()` |
| readiness / cut-over ack | `ready` | `markReady()` / `markServing()` / `ackPrepared()` |

### Can the app restart its own process — unconditionally? Yes.

`reboot()` bumps `restart_nonce`; lode polls `state.json`'s mtime (**~1 s**), sees
the bump and gracefully stops + respawns **your child process** (not lode/PID 1
itself). No precondition — call it for any reason or none: a self-detected resource/
memory leak, a periodic recycle, a watchdog. lode honours it in **every** phase
(Run → restart; Prepare → abandon the staged prepare & restart, pending target
survives; Observe → restart but keep the rollback window; Paused → resume), and
exactly **once per increment** (the value must rise — the SDK always increments).
Mind two things: there's no rate-limit, so don't bump in a hot loop; and in the
steady **Run** phase the restart re-reads `lode.toml`, which is exactly what the
`reloadConfig()` alias is for.

### Config reload, end to end

lode never auto-restarts a running app on a `lode.toml` edit. Instead it bumps
`config_generation` to *notify* you; you apply it on your own schedule:

```ts
lode.watch({ onConfigChange: (gen) => {
  // e.g. wait for in-flight work to finish, then:
  lode.reloadConfig();          // == reboot(); the relaunch re-reads lode.toml
}});
```

(Host-process env — container `-e` / k8s — still requires restarting lode itself;
only `lode.toml` `[env]`/config is picked up by an app restart.)

### Maintenance hold (don't auto-start)

When something must happen *before* the app comes up — a DB migration needing CLI
work, say — `hold()` tells lode to stop (re)starting and report `status="held"`. It
gates a *start*, not a running child, so to take a running app down for maintenance,
hold then exit yourself; lode holds instead of respawning:

```ts
lode.hold();              // lode won't (re)start the process
process.exit(0);          // take this instance down → lode stays held (does not respawn)
// ... operator/CLI runs the migration; a hold present at boot is also honoured ...
lode.release();           // lode starts the app again
```

An external operator/CLI can do the same against any data dir
(`new Lode({ lodeDir }).hold()` — readiness needs the child's instance id, but
hold/release don't).

## Quickstart

### TypeScript / Bun

```ts
import { Lode, onTerminate, isSupervised } from "./lode.ts";

const lode = Lode.fromEnv();        // reads LODE_DIR / LODE_INSTANCE

onTerminate(async () => { await drain(); });   // SIGTERM → drain → exit(0)

// after your server can actually serve:
if (isSupervised()) lode.markReady();

// drive lode from anywhere:
lode.requestUpdate("1.5.0");        // ask for an upgrade (lode installs it)
lode.reboot();                      // restart my own process (e.g. on a resource leak)
lode.rollback();                    // back to last_good

// subscribe to lode's notifications:
lode.watch({
  onConfigChange: (gen) => lode.reloadConfig(),       // operator edited lode.toml — apply when ready
  onAvailable: (v) => lode.requestUpdate(v),          // newer version advertised (policy = check)
  onPrepare: async () => { await checkpoint(); lode.ackPrepared(); },
});
```

### Go

```go
import "yourmod/lode" // wherever you dropped lode.go

c, err := lode.FromEnv()
if err != nil { /* not under lode */ }

lode.OnTerminate(func() { drain() })   // SIGTERM → drain → exit(0)
c.MarkReady()                          // once you can serve

c.RequestUpdate("1.5.0")
c.Reboot()                             // restart my own process (e.g. on a resource leak)
c.Rollback("")                         // "" ⇒ last_good

stop := make(chan struct{})
go c.Watch(stop, time.Second, lode.Handlers{
    OnConfigChange: func(gen uint64, s *lode.State) { c.ReloadConfig() },
    OnAvailable:    func(v string, s *lode.State) { c.RequestUpdate(v) },
    OnPrepare:      func(s *lode.State) { checkpoint(); c.AckPrepared() },
})
```

### Rust

`Cargo.toml`: `serde = { version = "1", features = ["derive"] }` and `serde_json = "1"`.
Then `mod lode;` and:

```rust
let lode = lode::Lode::from_env()?;

lode::install_term_handler();          // poll lode::terminating() in your loop
lode.mark_ready()?;                    // once you can serve

lode.request_update("1.5.0")?;
lode.reboot()?;                        // restart my own process (e.g. on a resource leak)
lode.rollback(None)?;                  // None ⇒ last_good

// subscribe to lode's notifications (blocking — run on its own thread):
let stop = std::sync::atomic::AtomicBool::new(false);
lode.watch(std::time::Duration::from_secs(1), &stop, lode::Handlers {
    on_config_change: Some(Box::new(|_gen, _s| { let _ = lode.reload_config(); })),
    on_available:     Some(Box::new(|v, _s| { let _ = lode.request_update(v); })),
    on_prepare:       Some(Box::new(|_s| { checkpoint(); let _ = lode.ack_prepared(); })),
    ..Default::default()
});
```

## Notes

- **Readiness only matters under `[supervise].readiness = "state"`.** In `none`
  mode lode uses a health-grace timer and ignores `ready`. Use the *phased*
  helpers (`markServing` / `ackPrepared`) only if you opt into the staged-update
  prepare handshake; otherwise a single `markReady()` is enough.
- **External ops tools** that aren't the supervised child can construct a client
  for any data dir (`new Lode({ lodeDir })` / `lode.New(dir, "")` / `Lode::new`)
  and issue `requestUpdate` / `reboot` / `rollback` — they just can't
  report readiness (that needs the child's `LODE_INSTANCE`).
- **Unix only** for Go/Rust (lode runs as PID 1). The TS SDK loads on any platform
  but is meant for the Unix lode runtime.
