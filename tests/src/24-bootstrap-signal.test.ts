// Scenario 24 (regression, P2-17) — signal handlers are installed BEFORE the
// bootstrap artifact download. In src/supervisor.rs `serve()`, `Signals::new(...)`
// is registered ahead of `resolve_target` (which bootstraps: fetch manifest +
// download/verify/install the artifact) and `ensure_runtime`. That ordering is what
// lets a SIGTERM arriving DURING a long bootstrap download be HANDLED by lode
// rather than fall through to the OS default disposition — the property that, when
// lode is PID 1 (`docker stop`), is the difference between a graceful shutdown and a
// hang until SIGKILL. Today the ordering is only a structural guarantee: nothing
// goes red if a refactor drops `Signals::new` below the download. This closes that
// gap behaviourally.
//
// How the bug surfaces here: the harness runs lode as a NORMAL (non-PID-1) process,
// so a SIGTERM with NO handler installed terminates it via the OS default (exit by
// signal). With the handler installed first, lode instead CATCHES SIGTERM during the
// download and exits on its OWN terms (a normal exit code, status=stopped). So the
// discriminator is "exited via signal" vs "exited gracefully" — not raw promptness.
//   fixed  : SIGTERM caught mid-download   -> exit { code: 0, signal: null }, stopped
//   broken : SIGTERM hits with no handler  -> exit { code: null, signal: "SIGTERM" }
// Asserting `signal === null` flips this test red the instant signal registration is
// moved below the download (verified by a scratch reorder; see the task report).

import { existsSync } from "node:fs";
import { createHash } from "node:crypto";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { buildApp } from "./helpers/app.ts";
import { Harness } from "./helpers/harness.ts";
import { mkTmp, rmTmp, sleep } from "./helpers/util.ts";

const APP_NAME = "e2e-slow-app";
const ASSET_NAME = "app.sh";
const VERSION = "0.0.1";

// Drip the artifact in this many chunks, sleeping between each, so the whole
// bootstrap download takes ~DRIP_CHUNKS*DRIP_MS and stays genuinely in progress
// while we send SIGTERM. Each chunk arrives well inside lode's 30s recv window
// (src/http.rs), so the download never times out — it is simply slow.
const DRIP_CHUNKS = 25;
const DRIP_MS = 150;

let h: Harness;
let slow: ReturnType<typeof Bun.serve> | undefined;
let artifactTmp: string | undefined;

afterEach(async () => {
  await h?.dispose();
  slow?.stop(true);
  slow = undefined;
  if (artifactTmp) {
    rmTmp(artifactTmp);
    artifactTmp = undefined;
  }
});

test("SIGTERM during the bootstrap download is honored (handler installed before fetch)", async () => {
  // Fresh data dir (Harness gives us one + lode-process tracking/cleanup). We do
  // NOT use its ManifestServer: it serves artifacts whole-file, and we need a
  // server that TRICKLES the artifact so the download stays in progress.
  h = await Harness.start();

  // Build a real artifact and serve those exact bytes (sha256 declared in the
  // manifest, so lode's integrity check passes even with require_signature=off —
  // this test is about signal timing, not crypto).
  artifactTmp = mkTmp("lode-slow-artifact-");
  const artifactPath = join(artifactTmp, ASSET_NAME);
  buildApp(artifactPath, { version: VERSION, mode: "service" });
  const bytes = new Uint8Array(await Bun.file(artifactPath).arrayBuffer());
  const sha256 = createHash("sha256").update(bytes).digest("hex");

  // Split the bytes into DRIP_CHUNKS roughly-equal pieces to enqueue one at a time.
  const chunks: Uint8Array[] = [];
  const step = Math.max(1, Math.ceil(bytes.length / DRIP_CHUNKS));
  for (let i = 0; i < bytes.length; i += step) chunks.push(bytes.subarray(i, i + step));

  // A custom loopback server: manifest.json is served instantly; the artifact is
  // streamed chunk-by-chunk with a sleep between, so the download lasts seconds.
  slow = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    async fetch(req) {
      const url = new URL(req.url);
      if (url.pathname === "/manifest.json") {
        const manifest = {
          schema: "lode/v1",
          name: APP_NAME,
          channels: { stable: { latest: VERSION } },
          versions: {
            [VERSION]: {
              min_lode: "0.0.1",
              notes: "e2e slow-drip artifact",
              assets: [{ name: ASSET_NAME, url: `${url.origin}/artifact/${ASSET_NAME}`, sha256 }],
            },
          },
        };
        return new Response(JSON.stringify(manifest), { headers: { "content-type": "application/json" } });
      }
      if (url.pathname === `/artifact/${ASSET_NAME}`) {
        const body = new ReadableStream<Uint8Array>({
          async start(controller) {
            try {
              for (const chunk of chunks) {
                controller.enqueue(chunk);
                await sleep(DRIP_MS);
              }
              controller.close();
            } catch {
              // Client (lode) went away mid-drip — expected when it is killed/stops.
            }
          },
        });
        return new Response(body, { headers: { "content-type": "application/octet-stream" } });
      }
      return new Response("not found", { status: 404 });
    },
  });
  const base = `http://127.0.0.1:${slow.port}`;

  // Drive lode directly at the slow server. No installed `current`, so it MUST
  // bootstrap-download before any child can spawn. require_signature=off keeps the
  // focus on signal timing (sha256 is still enforced via the declared digest).
  const lode = h.runLodeRaw([
    "--app",
    APP_NAME,
    "--dir",
    h.dataDir,
    "--manifest",
    `${base}/manifest.json`,
    "--asset",
    ASSET_NAME,
    // Literal launch command (the `{entry}` template is abolished): a raw artifact
    // lands under its own filename in the version dir. The child is never launched
    // (SIGTERM lands mid-download), so this value only needs to parse.
    "--run",
    `./${ASSET_NAME}`,
    "--exec",
    `./${ASSET_NAME}`,
    "--require-signature",
    "off",
    "--policy",
    "off",
    "--readiness",
    "none",
    "--log-level",
    "info",
  ]);

  // Wait until the download is genuinely in progress: lode streams to
  // `downloads/<ver>/<asset>.part` (src/download.rs), and no version is installed
  // yet, so no child exists. This anchors the SIGTERM firmly mid-download.
  const partPath = join(h.dataDir, "downloads", VERSION, `${ASSET_NAME}.part`);
  const deadline = Date.now() + 10000;
  while (!existsSync(partPath)) {
    if (Date.now() > deadline) throw new Error(`bootstrap .part never appeared:\n${lode.stderr}\n${lode.stdout}`);
    if (lode.exited) throw new Error(`lode exited before download started:\n${lode.stderr}\n${lode.stdout}`);
    await sleep(50);
  }
  // No child has spawned (still bootstrapping) — the window the handler must cover.
  expect(lode.readState()?.current ?? null).toBeNull();
  expect(lode.exited).toBe(false);
  // A touch more, so we are unambiguously mid-drip (download takes ~3.75s total).
  await sleep(300);
  expect(lode.exited).toBe(false);

  // SIGTERM while the artifact is still trickling in.
  lode.signal("SIGTERM");

  // lode must exit on its OWN terms (handler ran) — NOT be terminated by the OS
  // default SIGTERM disposition. `signal === null` is the discriminator that goes
  // red if signal registration is moved below the download.
  const exit = await lode.waitExit(15000);
  expect(exit.signal).toBeNull();
  expect(exit.code).toBe(0);
  // The graceful bootstrap-shutdown path recorded the terminal status.
  expect(lode.readState()?.status).toBe("stopped");
});
