// E3 — docker-compose end-to-end integration test for lode.
//
// Proves a REAL distroless lode image (built here from the repo Dockerfile)
// end to end: two lode service containers load the two example apps from a LOCAL
// signed manifest/artifact server, auto-update v0.0.1 -> v0.0.2, single-strike roll
// back from a crashing v0.0.3, do an update-by-app-exit, and a third service bounds
// an opt-in `restart=always` crash loop then exits. All local + deterministic — no
// real network, no GitHub, signature verification ENFORCED.
//
// Bun + TypeScript ONLY (the project constraint): every step is driven from this
// file via Bun.spawn of `cargo` / `docker` / `docker compose` / `lode` — no shell
// scripts. It is GATED on docker: when docker is absent the whole suite self-skips
// so non-docker runs (and the cargo gate) still pass.
//
// Why docker cp / docker exec instead of bind-mounts + published ports: the test
// must pass both on a normal docker host (CI) AND in docker-out-of-docker sandboxes
// where the test process shares the daemon socket but NOT its network/mount
// namespaces (there, host bind-mounts share the wrong files and published ports are
// unreachable). `docker cp` (file exchange) and `docker exec` (probing the services
// container-to-container, by IP, via the fileserver's built-in `lodetest get`) work
// in both. See tests/compose/docker-compose.yml.
//
// Host-level (non-docker) coverage of update-by-app-exit and "auto-update of a
// running app is treated as an update, not a crash" lives in E2's suite
// (tests/src/11-update-on-exit.test.ts, tests/src/12-auto-update-running.test.ts);
// this file exercises the same paths inside real containers and does not duplicate.
//
// Concurrency isolation: many checkouts/worktrees of this repo may run this suite
// at the same time against the SHARED docker daemon. Every daemon-global
// identifier — compose project, network name, subnet, fileserver IP, image tags —
// is therefore derived from a hash of the worktree path: STABLE per worktree (so
// a crashed run's leftovers are reclaimed by the next run here) and unique across
// worktrees (so concurrent runs never tear down or rebuild each other's stacks).
// The derived values reach docker-compose.yml via LODE_E2E_* env interpolation,
// and the committed lode.toml fixtures (which hard-code the default fileserver
// IP) are rewritten to the per-run IP before being `docker cp`ed in.

import { afterAll, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { Signer } from "../helpers/sign.ts";
import { baseEnv, flipHex, sleep } from "../helpers/util.ts";

// --- constants -------------------------------------------------------------

const REPO_ROOT = resolve(import.meta.dir, "../../..");
const COMPOSE_FILE = join(REPO_ROOT, "tests/compose/docker-compose.yml");
const COMPOSE_DIR = join(REPO_ROOT, "tests/compose");

// Per-worktree isolation suffix + derived daemon-global identifiers (see header).
const RUN_HASH = createHash("md5").update(REPO_ROOT).digest();
const SUFFIX = RUN_HASH.toString("hex").slice(0, 8);
const PROJECT = `lode_e2e_${SUFFIX}`;
const NETWORK = `lode_e2e_net_${SUFFIX}`;
const LODE_IMAGE = `lode:e2e-${SUFFIX}`;
const FS_IMAGE = `lode-fileserver:e2e-${SUFFIX}`;
// Subnet 10.<100..199>.<0..255>.0/24 — derived, so concurrent runs don't collide
// on overlapping address pools, and clear of docker's 172.16/12 + 192.168/16
// defaults. The fileserver keeps the fixed .2 host address inside it (static
// distroless binaries reach it by literal IP — no DNS/NSS).
const SUBNET_BASE = `10.${100 + (RUN_HASH[0]! % 100)}.${RUN_HASH[1]!}`;
const SUBNET = `${SUBNET_BASE}.0/24`;
const SERVER_IP = `${SUBNET_BASE}.2`; // per-run fileserver IP (interpolated into compose + lode.toml)
const DEFAULT_SERVER_IP = "10.123.231.2"; // the standalone default hard-coded in the lode.toml fixtures
const SERVER_PORT = 8080;
const ARCH = process.arch === "arm64" ? "aarch64" : "x86_64";
const DOCKER_ARCH = process.arch === "arm64" ? "arm64" : "amd64";
const TRIPLE = `${ARCH}-unknown-linux-gnu`;
const STATIC_RUSTFLAGS = "-C target-feature=+crt-static"; // CI-portable static (no musl-tools)
const CARGO = process.env.CARGO ?? "cargo"; // CI has cargo on PATH; else set $CARGO

// Generous because this builds 4 static binaries + 2 images, brings the stack up,
// and drives several update/rollback transitions. Overrides bun's --timeout.
const E2E_TIMEOUT = 900_000;

// --- docker availability (sync, at module load) ----------------------------

function hasDocker(): boolean {
  try {
    const a = Bun.spawnSync(["docker", "version"], { stdout: "ignore", stderr: "ignore" });
    const b = Bun.spawnSync(["docker", "compose", "version"], { stdout: "ignore", stderr: "ignore" });
    return a.exitCode === 0 && b.exitCode === 0;
  } catch {
    return false;
  }
}
const DOCKER = hasDocker();

// --- small process + temp helpers ------------------------------------------

interface ShResult {
  exitCode: number | null;
  stdout: string;
  stderr: string;
}

async function sh(cmd: string[], opts: { cwd?: string; env?: Record<string, string> } = {}): Promise<ShResult> {
  const proc = Bun.spawn({
    cmd,
    cwd: opts.cwd,
    env: opts.env ?? baseEnv(),
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  return { exitCode, stdout, stderr };
}

const cleanup: string[] = [];
function tmp(prefix: string): string {
  const d = mkdtempSync(join(tmpdir(), prefix));
  cleanup.push(d);
  return d;
}

function log(msg: string): void {
  // eslint-disable-next-line no-console
  console.log(`[compose-e2e] ${msg}`);
}

// --- build helpers ---------------------------------------------------------

async function cargoBuildStatic(cwd: string, bin: string, extraEnv: Record<string, string> = {}): Promise<string> {
  const r = await sh([CARGO, "build", "--release", "--target", TRIPLE, "--locked"], {
    cwd,
    env: { ...baseEnv(), RUSTFLAGS: STATIC_RUSTFLAGS, ...extraEnv },
  });
  if (r.exitCode !== 0) throw new Error(`cargo build ${bin} (${cwd}) failed:\n${r.stderr}\n${r.stdout}`);
  return join(cwd, "target", TRIPLE, "release", bin);
}

/** Bake a versioned web-bun script (the served artifact) from tests/apps/web-bun/app.ts. */
function makeBunScript(version: string, bad: boolean): string {
  const src = readFileSync(join(REPO_ROOT, "tests/apps/web-bun/app.ts"), "utf8");
  return src
    .replace(/^const BUILD_VERSION = .*$/m, `const BUILD_VERSION = "${version}";`)
    .replace(/^const BUILD_BAD = .*$/m, `const BUILD_BAD = "${bad ? "1" : "0"}";`);
}

// --- local lode/v1 registry (one manifest per app name) --------------------

// One per-version asset, keyed by its filename (`name`) — the source-agnostic
// selection key (§3) and the §1 signed identity. `format` is derived from the
// filename at install time, so it is neither stored nor signed.
interface Asset {
  name: string;
  url: string;
  sha256: string;
  sig?: string;
  key_id?: string;
}
interface Manifest {
  schema: "lode/v1";
  name: string;
  channels: Record<string, { latest: string }>;
  versions: Record<string, { min_lode: string; notes: string; assets: Asset[] }>;
}

interface PublishOpts {
  srcPath: string;
  /** The served asset filename — the selection key (`[update].asset`), the
   *  basename the signature binds, and where the raw artifact lands (each
   *  service's lode.toml [command] names it, e.g. run = "./web-rust"). */
  asset: string;
  latest?: boolean;
  tamperSha?: boolean;
  omitSig?: boolean;
}

class Registry {
  /** Local staging tree; its basename MUST be `www` so the initial docker cp lands /www. */
  readonly www: string;
  readonly #signer: Signer;
  readonly #manifests = new Map<string, Manifest>();
  #fsCid = "";

  constructor(root: string, signer: Signer) {
    this.www = join(root, "www");
    mkdirSync(this.www, { recursive: true });
    this.#signer = signer;
  }

  setFileserver(cid: string): void {
    this.#fsCid = cid;
  }

  #manifest(app: string): Manifest {
    let m = this.#manifests.get(app);
    if (!m) {
      m = { schema: "lode/v1", name: app, channels: {}, versions: {} };
      this.#manifests.set(app, m);
    }
    return m;
  }

  /** Build (copy), sign, and register a version; rewrites the app's manifest.json.
   *  The asset is served — and keyed in the manifest — under `opts.asset`, which is
   *  the filename each app's lode.toml names via `[update].asset`. */
  async publish(app: string, version: string, opts: PublishOpts): Promise<void> {
    const dir = join(this.www, app, "artifacts", version);
    mkdirSync(dir, { recursive: true });
    const dest = join(dir, opts.asset);
    copyFileSync(opts.srcPath, dest);
    chmodSync(dest, 0o755);

    // The signature binds the asset filename (= basename(dest) = opts.asset) +
    // version + sha256 (+ run/exec overrides, unused here — each service's
    // lode.toml [command] drives the launch); the url is never signed.
    const url = `http://${SERVER_IP}:${SERVER_PORT}/${app}/artifacts/${version}/${opts.asset}`;
    const signed = await this.#signer.sign(dest, version);
    const sha256 = opts.tamperSha ? flipHex(signed.sha256) : signed.sha256;
    const asset: Asset = {
      name: opts.asset,
      url,
      sha256,
    };
    if (!opts.omitSig) {
      asset.sig = signed.sig;
      asset.key_id = this.#signer.keyId;
    }
    const m = this.#manifest(app);
    m.versions[version] = { min_lode: "0.0.1", notes: `e2e ${version}`, assets: [asset] };
    if (opts.latest ?? true) m.channels.stable = { latest: version };
    await this.#writeManifest(app);
  }

  /** Place the (unsigned — runtimes carry no signature) stub bun runtime at /www/runtime/bun. */
  putRuntime(srcPath: string): void {
    const d = join(this.www, "runtime");
    mkdirSync(d, { recursive: true });
    const dest = join(d, "bun");
    copyFileSync(srcPath, dest);
    chmodSync(dest, 0o755);
  }

  async #writeManifest(app: string): Promise<void> {
    const dir = join(this.www, app);
    mkdirSync(dir, { recursive: true });
    const path = join(dir, "manifest.json");
    writeFileSync(path, JSON.stringify(this.#manifest(app), null, 2));
    // The loader verifies the manifest-level signature (svc-* use enforce + a
    // trusted key), so stamp the top-level key_id + sig over the catalog as it
    // now stands — re-signed on every republish.
    await this.#signer.signManifest(path);
  }

  /** Copy the whole tree into the (created, not-yet-started) fileserver as /www. */
  async syncInitial(): Promise<void> {
    const r = await sh(["docker", "cp", this.www, `${this.#fsCid}:/`]);
    if (r.exitCode !== 0) throw new Error(`initial www sync failed: ${r.stderr}`);
  }

  /** Merge the current tree into the running fileserver's /www (publish at runtime). */
  async sync(): Promise<void> {
    const r = await sh(["docker", "cp", `${this.www}/.`, `${this.#fsCid}:/www`]);
    if (r.exitCode !== 0) throw new Error(`www sync failed: ${r.stderr}`);
  }
}

// --- docker / compose helpers ----------------------------------------------

function composeEnv(trustedKey: string): Record<string, string> {
  return {
    ...baseEnv(),
    TRUSTED_KEY: trustedKey,
    // Per-run identifiers interpolated by docker-compose.yml (isolation, see header).
    LODE_E2E_NET: NETWORK,
    LODE_E2E_SUBNET: SUBNET,
    LODE_E2E_SERVER_IP: SERVER_IP,
    LODE_E2E_IMAGE: LODE_IMAGE,
    LODE_E2E_FS_IMAGE: FS_IMAGE,
  };
}

let TRUSTED = "";
async function dc(args: string[]): Promise<ShResult> {
  return sh(["docker", "compose", "-p", PROJECT, "-f", COMPOSE_FILE, ...args], { env: composeEnv(TRUSTED) });
}

async function cidOf(svc: string): Promise<string> {
  const r = await dc(["ps", "-aq", svc]);
  return r.stdout.trim().split("\n")[0] ?? "";
}

async function ipOf(cid: string): Promise<string> {
  const r = await sh(["docker", "inspect", cid, "--format", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}"]);
  return r.stdout.trim();
}

interface ContainerState {
  running: boolean;
  exitCode: number;
}
async function containerState(cid: string): Promise<ContainerState> {
  const r = await sh(["docker", "inspect", cid, "--format", "{{.State.Running}} {{.State.ExitCode}}"]);
  const [running, code] = r.stdout.trim().split(/\s+/);
  return { running: running === "true", exitCode: Number(code ?? "0") };
}

async function logsOf(cid: string): Promise<string> {
  const r = await sh(["docker", "logs", cid]);
  return r.stdout + r.stderr;
}

function countMatches(text: string, re: RegExp): number {
  return (text.match(re) ?? []).length;
}

/** Probe an app's HTTP endpoint container-to-container, via the fileserver's get client. */
async function probe(fsCid: string, ip: string, path: string): Promise<{ ok: boolean; body: string }> {
  const r = await sh(["docker", "exec", fsCid, "/lodetest", "get", `http://${ip}:${SERVER_PORT}${path}`]);
  return { ok: r.exitCode === 0, body: r.stdout };
}

interface LodeState {
  current?: string;
  last_good?: string;
  status?: string;
  target?: string;
  history?: { version: string; result: string }[];
  last_error?: string;
}
async function readState(cid: string): Promise<LodeState | null> {
  const dir = tmp("lode-state-");
  const local = join(dir, "state.json");
  const r = await sh(["docker", "cp", `${cid}:/data/state.json`, local]);
  if (r.exitCode !== 0) return null;
  try {
    return JSON.parse(readFileSync(local, "utf8")) as LodeState;
  } catch {
    return null;
  }
}

/** Read-modify-write `state.target` into the container (an app/operator update request). */
async function writeTarget(cid: string, version: string): Promise<void> {
  const st = (await readState(cid)) ?? {};
  (st as Record<string, unknown>).target = version;
  const dir = tmp("lode-target-");
  const local = join(dir, "state.json");
  writeFileSync(local, JSON.stringify(st, null, 2));
  await sh(["docker", "cp", local, `${cid}:/data/state.json`]);
}

/** Drop the update-by-app-exit trigger file the bun stub watches. */
async function dropExitTrigger(cid: string, version: string): Promise<void> {
  const dir = tmp("lode-trigger-");
  const local = join(dir, "please_exit_update");
  writeFileSync(local, version);
  await sh(["docker", "cp", local, `${cid}:/data/please_exit_update`]);
}

async function pollVersion(fsCid: string, cid: string, want: string, timeout = 60_000, label = ""): Promise<void> {
  const start = Date.now();
  let last = "";
  while (Date.now() - start < timeout) {
    // Re-derive the IP each poll: a container could restart (unless-stopped) and
    // get a new address; a stale IP would silently never match.
    const ip = await ipOf(cid);
    if (ip) {
      const { ok, body } = await probe(fsCid, ip, "/version");
      if (ok) last = body.trim();
      if (ok && body.trim() === want) return;
    }
    await sleep(800);
  }
  throw new Error(`pollVersion ${label}: never reached "${want}" (last="${last}")`);
}

async function pollState(
  cid: string,
  pred: (s: LodeState) => boolean,
  timeout = 60_000,
  label = "",
): Promise<LodeState> {
  const start = Date.now();
  let last: LodeState | null = null;
  while (Date.now() - start < timeout) {
    last = await readState(cid);
    if (last && pred(last)) return last;
    await sleep(700);
  }
  throw new Error(`pollState ${label}: predicate never held (last=${JSON.stringify(last)})`);
}

// --- the test --------------------------------------------------------------

const itDocker = test.skipIf(!DOCKER);

if (!DOCKER) {
  // Surface why the suite is green-without-running (CI has docker; this is for
  // local non-docker runs and the cargo-only gate).
  // eslint-disable-next-line no-console
  console.log("[compose-e2e] docker not available — skipping the docker-compose integration test");
}

afterAll(async () => {
  if (DOCKER) {
    try {
      await dc(["down", "-v", "--remove-orphans"]);
    } catch {
      // best effort
    }
    try {
      // Only THIS run's derived tags — never another worktree's images. The
      // buildkit layer cache survives, so the next run here rebuilds cheaply.
      await sh(["docker", "rmi", "-f", LODE_IMAGE, FS_IMAGE]);
    } catch {
      // best effort
    }
  }
  for (const d of cleanup) {
    try {
      rmSync(d, { recursive: true, force: true });
    } catch {
      // best effort
    }
  }
  try {
    rmSync(join(REPO_ROOT, `linux-${DOCKER_ARCH}`), { recursive: true, force: true });
  } catch {
    // best effort
  }
});

itDocker(
  "two lode containers: auto-update, single-strike rollback, update-by-exit, bounded restart",
  async () => {
    const root = tmp("lode-e2e-");
    log(
      `isolation: project=${PROJECT} network=${NETWORK} subnet=${SUBNET} ` +
        `fileserver=${SERVER_IP} images=${LODE_IMAGE},${FS_IMAGE}`,
    );

    // PHASE 0 — publisher key + registry -------------------------------------
    log("phase 0: keygen + registry");
    const keysDir = tmp("lode-keys-");
    const signer = await Signer.create(keysDir);
    TRUSTED = signer.trustedKey;
    const reg = new Registry(root, signer);

    // PHASE 1 — build the static binaries (CI-portable +crt-static) ----------
    log("phase 1: building static binaries (lode, lodetest, web-rust good/bad)");
    const lodeBin = await cargoBuildStatic(REPO_ROOT, "lode");
    const lodetestBin = await cargoBuildStatic(join(COMPOSE_DIR, "lodetest"), "lodetest");
    const webRustDir = join(REPO_ROOT, "tests/apps/web-rust");
    const webRustGood = join(root, "web-rust-good");
    const webRustBad = join(root, "web-rust-bad");
    copyFileSync(await cargoBuildStatic(webRustDir, "web-rust"), webRustGood);
    copyFileSync(await cargoBuildStatic(webRustDir, "web-rust", { BUILD_BAD: "1" }), webRustBad);

    // PHASE 2 — build the local images --------------------------------------
    log(`phase 2: docker build ${LODE_IMAGE} + ${FS_IMAGE}`);
    const stageDir = join(REPO_ROOT, `linux-${DOCKER_ARCH}`);
    mkdirSync(stageDir, { recursive: true });
    copyFileSync(lodeBin, join(stageDir, "lode"));
    const buildLode = await sh(["docker", "build", "-t", LODE_IMAGE, REPO_ROOT]);
    expect(buildLode.exitCode, `docker build ${LODE_IMAGE} failed:\n${buildLode.stderr}`).toBe(0);

    const fsCtx = tmp("lode-fsctx-");
    copyFileSync(lodetestBin, join(fsCtx, "lodetest"));
    const buildFs = await sh([
      "docker",
      "build",
      "-t",
      FS_IMAGE,
      "-f",
      join(COMPOSE_DIR, "fileserver/Dockerfile"),
      fsCtx,
    ]);
    expect(buildFs.exitCode, `docker build fileserver failed:\n${buildFs.stderr}`).toBe(0);

    // PHASE 3 — stage the initial v0.0.1 manifests + runtime -----------------
    log("phase 3: sign + stage v0.0.1 (web-rust, web-bun, crash) + stub bun runtime");
    reg.putRuntime(lodetestBin);
    await reg.publish("web-rust", "0.0.1", { srcPath: webRustGood, asset: "web-rust" });
    const bun001 = join(root, "app-0.0.1.ts");
    writeFileSync(bun001, makeBunScript("0.0.1", false));
    await reg.publish("web-bun", "0.0.1", { srcPath: bun001, asset: "app.ts" });
    // svc-restart's "crash" app: a crashing native binary (policy=off, never updates).
    await reg.publish("crash", "0.0.1", { srcPath: webRustBad, asset: "crashbin" });

    // PHASE 4 — create containers, cp config + registry, start ----------------
    log("phase 4: compose create + docker cp config/registry + start");
    await dc(["down", "-v", "--remove-orphans"]); // clean any prior run
    const created = await dc(["create"]);
    expect(created.exitCode, `compose create failed:\n${created.stderr}`).toBe(0);

    const fsCid = await cidOf("fileserver");
    const rustCid = await cidOf("svc-rust");
    const bunCid = await cidOf("svc-bun");
    const restartCid = await cidOf("svc-restart");
    expect(fsCid && rustCid && bunCid && restartCid, "all containers created").toBeTruthy();
    reg.setFileserver(fsCid);

    // Config: docker cp each committed lode.toml (LODE_CONFIG=/etc/lode.toml),
    // rewriting the fixture's default fileserver IP to this run's derived one.
    const cfgDir = tmp("lode-cfg-");
    for (const [svc, cid] of [
      ["svc-rust", rustCid],
      ["svc-bun", bunCid],
      ["svc-restart", restartCid],
    ] as const) {
      const toml = readFileSync(join(COMPOSE_DIR, svc, "lode.toml"), "utf8").replaceAll(DEFAULT_SERVER_IP, SERVER_IP);
      const local = join(cfgDir, `${svc}.toml`);
      writeFileSync(local, toml);
      const cp = await sh(["docker", "cp", local, `${cid}:/etc/lode.toml`]);
      expect(cp.exitCode, `cp config ${svc}: ${cp.stderr}`).toBe(0);
    }
    await reg.syncInitial();

    // Start fileserver first and wait until it serves, so lode never races a 404.
    await dc(["start", "fileserver"]);
    {
      const start = Date.now();
      let ready = false;
      while (Date.now() - start < 30_000) {
        const r = await sh(["docker", "exec", fsCid, "/lodetest", "get", `http://127.0.0.1:8080/web-rust/manifest.json`]);
        if (r.exitCode === 0) {
          ready = true;
          break;
        }
        await sleep(400);
      }
      expect(ready, "fileserver became ready").toBe(true);
    }
    await dc(["start", "svc-rust", "svc-bun", "svc-restart"]);

    const rustIp = await ipOf(rustCid);
    const bunIp = await ipOf(bunCid);
    log(`svc-rust=${rustIp} svc-bun=${bunIp}`);

    // PHASE 5 — both apps serve v0.0.1 ---------------------------------------
    log("phase 5: assert both apps report v0.0.1");
    await pollVersion(fsCid, rustCid,"0.0.1", 60_000, "svc-rust v0.0.1");
    await pollVersion(fsCid, bunCid,"0.0.1", 90_000, "svc-bun v0.0.1 (downloads stub bun runtime first)");

    // PHASE 6 — auto-update v0.0.1 -> v0.0.2 (policy=auto) --------------------
    log("phase 6: publish v0.0.2 (latest) -> both auto-update");
    await reg.publish("web-rust", "0.0.2", { srcPath: webRustGood, asset: "web-rust" });
    const bun002 = join(root, "app-0.0.2.ts");
    writeFileSync(bun002, makeBunScript("0.0.2", false));
    await reg.publish("web-bun", "0.0.2", { srcPath: bun002, asset: "app.ts" });
    await reg.sync();
    await pollVersion(fsCid, rustCid,"0.0.2", 60_000, "svc-rust auto v0.0.2");
    await pollVersion(fsCid, bunCid,"0.0.2", 60_000, "svc-bun auto v0.0.2");
    expect((await readState(rustCid))?.last_good).toBe("0.0.2");
    expect((await readState(bunCid))?.last_good).toBe("0.0.2");

    // PHASE 7 — crashing v0.0.3 -> single-strike rollback to v0.0.2 ----------
    // Published as a (signed) version but NOT channel-latest, then requested via
    // state.target, so policy=auto cannot re-apply it in a loop after rollback.
    log("phase 7: crashing v0.0.3 -> single-strike rollback to v0.0.2 (both)");
    await reg.publish("web-rust", "0.0.3", { srcPath: webRustBad, asset: "web-rust", latest: false });
    const bun003 = join(root, "app-0.0.3.ts");
    writeFileSync(bun003, makeBunScript("0.0.3", true));
    await reg.publish("web-bun", "0.0.3", { srcPath: bun003, asset: "app.ts", latest: false });
    await reg.sync();

    const rolledBack = async (cid: string, label: string): Promise<void> => {
      await writeTarget(cid, "0.0.3");
      const deadline = Date.now() + 50_000;
      // `applied` latches once lode shows ANY sign of having applied v0.0.3, after
      // which we never re-assert target again — so a rollback that cleared the
      // target can't be re-triggered into a loop by a stale read.
      let applied = false;
      while (Date.now() < deadline) {
        const s = await readState(cid);
        if (
          s &&
          s.status === "running" &&
          s.current === "0.0.2" &&
          (s.history ?? []).some((h) => h.version === "0.0.3" && h.result === "bad")
        ) {
          expect(s.last_good).toBe("0.0.2");
          expect((await containerState(cid)).running, `${label} lode still supervising after rollback`).toBe(true);
          return;
        }
        if (/0\.0\.3/.test(await logsOf(cid))) applied = true;
        if (!applied && s?.target !== "0.0.3") await writeTarget(cid, "0.0.3");
        await sleep(800);
      }
      throw new Error(`${label}: never observed single-strike rollback to v0.0.2 after crashing v0.0.3`);
    };
    await rolledBack(rustCid, "svc-rust");
    await rolledBack(bunCid, "svc-bun");
    await pollVersion(fsCid, rustCid,"0.0.2", 30_000, "svc-rust serving v0.0.2 post-rollback");
    await pollVersion(fsCid, bunCid,"0.0.2", 30_000, "svc-bun serving v0.0.2 post-rollback");

    // PHASE 8 — update-by-app-exit in-container (svc-bun) --------------------
    // The running app writes state.target then exits(0); lode applies the pending
    // update and relaunches DIRECTLY on the new version (no flap to the old one).
    log("phase 8: update-by-app-exit on svc-bun -> v0.0.5 (no flap)");
    const bun005 = join(root, "app-0.0.5.ts");
    writeFileSync(bun005, makeBunScript("0.0.5", false));
    await reg.publish("web-bun", "0.0.5", { srcPath: bun005, asset: "app.ts", latest: false });
    await reg.sync();
    const start002 = countMatches(await logsOf(bunCid), /\[bun\] starting version=0\.0\.2/g);
    await dropExitTrigger(bunCid, "0.0.5");
    await pollVersion(fsCid, bunCid,"0.0.5", 45_000, "svc-bun update-by-exit v0.0.5");
    const bunLogs = await logsOf(bunCid);
    expect(countMatches(bunLogs, /\[bun\] starting version=0\.0\.5/g), "v0.0.5 launched").toBeGreaterThanOrEqual(1);
    // No flap: lode did not relaunch the OLD v0.0.2 as part of this transition.
    expect(countMatches(bunLogs, /\[bun\] starting version=0\.0\.2/g), "no v0.0.2 relaunch (no flap)").toBe(start002);
    expect((await containerState(bunCid)).running, "svc-bun still up after update-by-exit").toBe(true);

    // PHASE 9 — restart=always bounds the crash loop then PAUSES (keep-alive) --
    log("phase 9: svc-restart bounded its restart loop then paused (status=error, still up)");
    const restartState = await pollState(restartCid, (s) => s.status === "error", 30_000, "svc-restart status=error");
    expect(restartState.status).toBe("error");
    expect(restartState.last_error ?? "", "svc-restart paused (not exited)").toMatch(/paused/i);
    // Keep-alive: lode does NOT exit — PID 1 stays alive so the container does not
    // crash-loop. The container is still running after the app gave up.
    const cs = await containerState(restartCid);
    expect(cs.running, "svc-restart container still up (lode paused, did not exit)").toBe(true);
    const restartLogs = await logsOf(restartCid);
    // Bounded: the app launched and crashed more than once (initial + restarts),
    // then lode paused at the cap.
    expect(countMatches(restartLogs, /\[web-rust\] bad mode/g), "crashing app restarted (bounded)").toBeGreaterThanOrEqual(2);
    expect(restartLogs, "lode reported pausing after the retry cap").toMatch(/pausing/);

    // PHASE 10 — clean teardown ----------------------------------------------
    log("phase 10: docker compose down -v");
    const down = await dc(["down", "-v", "--remove-orphans"]);
    expect(down.exitCode, `down failed:\n${down.stderr}`).toBe(0);
    const remaining = await dc(["ps", "-aq"]);
    expect(remaining.stdout.trim(), "no containers remain after down -v").toBe("");
  },
  E2E_TIMEOUT,
);
