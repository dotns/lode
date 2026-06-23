// lode demo (Bun + TypeScript). See ../README.md.
//
// Conforms to the lode app contract via the SDK ([../../sdks/lode.ts]) and shows
// the three things an app does under lode:
//   1. START   — Bun.serve on $PORT; lode runs `bun run app.js` as its child.
//   2. READ    — read lode-injected env via the SDK (activeVersion / instanceId /
//                dataDir) + passthrough host env (PORT, operator [env]).
//   3. UPGRADE — (a) PASSIVE: markReady() + onTerminate(), so lode's update/
//                rollback is seamless; (b) ACTIVE: the endpoints below call
//                requestUpdate / reboot / hold / release.
//
// Bundle to ONE file with `bun run package.ts` (-> dist/app.js); that single file
// is the artifact lode installs (asset = "app.js"), run with `run = "bun run"`.
// The bundler inlines the SDK, so dist/app.js is self-contained.

import { activeVersion, dataDir, instanceId, isSupervised, Lode, onTerminate } from "../../sdks/lode.ts";

// BUILD_VERSION is inlined by package.ts (`--define`); absent when run unbundled.
declare const BUILD_VERSION: string;
const baked = typeof BUILD_VERSION === "string" ? BUILD_VERSION : "0.0.0-dev";

// lode's LODE_ACTIVE_VERSION wins so /version matches what lode installed.
const version = activeVersion() ?? baked;
const port = Number(Bun.env.PORT ?? "8080");

// The SDK handle — null when run standalone (no lode / LODE_DATA_DIR unset).
const lode = isSupervised() ? Lode.fromEnv() : null;

const log = (m: string): void => console.log(`[demo-bun] ${m}`);

// `lode version` passthrough (exec = "bun" → `bun app.js version`).
if (["version", "--version", "-v"].includes(Bun.argv[2] ?? "")) {
  console.log(version);
  process.exit(0);
}

const text = (body: string, status = 200): Response =>
  new Response(body, { status, headers: { "content-type": "text/plain; charset=utf-8" } });
const json = (body: unknown): Response =>
  new Response(JSON.stringify(body, null, 2), { status: 200, headers: { "content-type": "application/json" } });

// Run an SDK request, or 503 when not supervised by lode.
const ask = (fn: (l: Lode) => void, ok: string): Response =>
  lode ? (fn(lode), text(`${ok}\n`)) : text("not running under lode (LODE_DATA_DIR unset)\n", 503);

const server = Bun.serve({
  port,
  fetch(req): Response {
    const { pathname } = new URL(req.url);
    switch (pathname) {
      case "/healthz":
        return text("ok\n");
      case "/version":
        return text(`${version}\n`);
      case "/env": // READ
        return json({
          version, // LODE_ACTIVE_VERSION or baked
          instance: instanceId(), // unique id per launch
          dataDir: dataDir() ?? null, // where state.json lives
          port, // host env passthrough
          greeting: Bun.env.APP_GREETING ?? null, // operator [env] / host -e
        });
      case "/upgrade": // UPGRADE (active): ask lode to pull latest
        return ask((l) => l.requestUpdate("latest"), "requested update to latest");
      case "/restart": // UPGRADE (active): restart this version
        return ask((l) => l.reboot(), "requested restart");
      case "/hold": // MAINTENANCE: ask lode not to (re)start the process
        return ask((l) => l.hold(), "held (lode will not (re)start the app)");
      case "/release": // MAINTENANCE: clear the hold
        return ask((l) => l.release(), "released");
      default:
        return text("not found\n", 404);
    }
  },
});

log(
  `starting version=${version} pid=${process.pid} instance=${instanceId() || "none"} ` +
    `data_dir=${dataDir() ?? "unset"} addr=0.0.0.0:${server.port}`,
);

// UPGRADE (passive): graceful stop — drain and exit(0) within supervise.stop_timeout.
onTerminate(() => {
  log("shutting down");
  server.stop(true);
});

// UPGRADE (passive): announce readiness so lode (readiness="state") commits us.
if (lode) {
  lode.markReady();
  log(`ready: state.ready=${instanceId()}`);
} else {
  log("readiness skipped (standalone)");
}
