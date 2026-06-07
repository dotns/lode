#!/usr/bin/env bun
// web-bun — a minimal Bun + TypeScript HTTP service used by lode's integration
// tests. It implements the SAME language-agnostic lode app contract as
// the sibling `tests/apps/web-rust`, using
// ONLY Bun built-ins (Bun.serve, node:fs) — zero external dependencies.
//
// Contract (design §4/§7/§8, integration §2):
//   * binds an HTTP server to $PORT (default 8080)
//   * GET /version -> the app's own version (plain text); GET /healthz -> 200 ok
//   * version: LODE_ACTIVE_VERSION (injected by lode) wins, else baked BUILD_VERSION
//   * graceful stop: SIGTERM/SIGINT -> drain + exit(0) sub-second (< stop_timeout)
//   * readiness: when LODE_DATA_DIR is set, atomically (temp + rename) writes
//     state.json field ready = $LODE_INSTANCE, preserving lode's fields
//   * bad mode: baked BUILD_BAD=1 or runtime LODE_APP_BAD=1 -> exit(1) on startup

import { existsSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

// Baked at package time by build.sh (it rewrites these two lines). At runtime,
// lode's injected LODE_ACTIVE_VERSION wins over BUILD_VERSION (below).
const BUILD_VERSION = "0.0.0-dev";
const BUILD_BAD = "0";

const active = Bun.env.LODE_ACTIVE_VERSION;
const version = active && active.length > 0 ? active : BUILD_VERSION;

const log = (msg: string): void => console.log(`[web-bun] ${msg}`);

// `bun app.ts version` (or --version/-v) just prints the version and exits 0 —
// handy for standalone testing and `lode version` passthrough.
const arg = Bun.argv[2];
if (arg === "version" || arg === "--version" || arg === "-v") {
  console.log(version);
  process.exit(0);
}

// Bad mode (rollback testing): crash immediately so the new version never
// survives health_grace and lode rolls back. Baked ("bad v0.0.3" artifact) or
// forced at runtime via LODE_APP_BAD=1 without rebuilding.
const runtimeBad = Bun.env.LODE_APP_BAD === "1";
if (BUILD_BAD === "1" || runtimeBad) {
  console.error(`[web-bun] bad mode (baked=${BUILD_BAD === "1"} LODE_APP_BAD=${runtimeBad}) — crashing on startup, exit 1`);
  process.exit(1);
}

const port = Number(Bun.env.PORT ?? "8080");

// Graceful-stop contract (lode -> app: SIGTERM). Clean up and exit(0) well
// within supervise.stop_timeout (default 10s) or get SIGKILLed.
let shuttingDown = false;
const shutdown = (sig: string): void => {
  if (shuttingDown) return;
  shuttingDown = true;
  log(`${sig} received — cleaning up`);
  server.stop(true); // stop accepting + close active connections
  log("cleanup done, exiting 0");
  process.exit(0);
};
process.on("SIGTERM", () => shutdown("SIGTERM"));
process.on("SIGINT", () => shutdown("SIGINT"));

const text = (body: string, status = 200): Response =>
  new Response(body, { status, headers: { "content-type": "text/plain; charset=utf-8" } });

const server = Bun.serve({
  port,
  fetch(req): Response {
    const { pathname } = new URL(req.url);
    if (pathname === "/version") return text(version);
    if (pathname === "/healthz") return text("ok");
    return text("not found", 404);
  },
});

log(
  `starting version=${version} pid=${process.pid} instance=${Bun.env.LODE_INSTANCE ?? "none"} ` +
    `data_dir=${Bun.env.LODE_DATA_DIR ?? "unset"} addr=0.0.0.0:${server.port}`,
);

// Readiness handshake (app -> lode). Once the port is bound we can serve, so
// announce: write state.ready = LODE_INSTANCE atomically, preserving lode's
// fields. No-op when LODE_DATA_DIR is unset (standalone runs).
announceReady();

function announceReady(): void {
  const dataDir = Bun.env.LODE_DATA_DIR;
  if (!dataDir) return;
  const inst = Bun.env.LODE_INSTANCE ?? "";
  const statePath = join(dataDir, "state.json");

  let state: Record<string, unknown> = {};
  if (existsSync(statePath)) {
    try {
      state = JSON.parse(readFileSync(statePath, "utf8")) as Record<string, unknown>;
    } catch {
      state = {}; // unreadable/corrupt -> write a minimal valid object
    }
  }
  state.ready = inst;

  const tmp = `${statePath}.ready.${process.pid}`;
  try {
    writeFileSync(tmp, `${JSON.stringify(state, null, 2)}\n`);
    renameSync(tmp, statePath); // atomic replace
  } catch (e) {
    try {
      unlinkSync(tmp);
    } catch {
      /* best-effort cleanup */
    }
    console.error(`[web-bun] ready write failed: ${String(e)}`);
    return;
  }
  log(`ready: wrote state.ready=${inst} -> ${statePath}`);
}
