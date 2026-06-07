// Scenario 14 — path traversal (security P0): a manifest `versions` key / channel
// `latest` that names a traversal id like "../../pwned" must be REFUSED before it
// ever reaches a filesystem path. We drive the real lode binary two ways — a
// `lode-cli update` (manifest resolution) and a bare-lode bootstrap (supervisor
// serve) — and assert: (a) the command fails non-zero with a clear error, (b) NO
// file/dir leaks outside the data dir (the parent stays free of the escape name),
// and (c) a pre-existing `current` version is left untouched.

import { afterEach, expect, test } from "bun:test";
import { existsSync, readdirSync, readFileSync, readlinkSync } from "node:fs";
import { dirname, join } from "node:path";

import { Harness } from "./helpers/harness.ts";
import { LodeRunner } from "./helpers/lode.ts";
import { baseEnv, LODE_CLI_BIN, mkTmp, rmTmp, run } from "./helpers/util.ts";

const APP_NAME = "e2e-app";

interface MaliciousServer {
  manifestUrl: string;
  stop(): void;
}

/**
 * Serve a structurally-valid `lode/v1` manifest whose channel `latest` and sole
 * `versions` key are `version` (a traversal id). A reachable `/payload` is served
 * too, so that — absent the fix — lode would actually write `<leak>.part` outside
 * the data dir (making the leak assertions meaningful, not vacuous). The schema is
 * valid so the rejection is the version-id traversal guard (not a schema error).
 */
function startMaliciousServer(appName: string, version: string): MaliciousServer {
  const manifest = {
    schema: "lode/v1",
    name: appName,
    channels: { stable: { latest: version } },
    versions: {
      [version]: {
        min_lode: "0.0.1",
        notes: "malicious",
        assets: [{ name: "app", url: "", sha256: "00".repeat(32), entry: "app" }],
      },
    },
  };
  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    fetch(req) {
      const { pathname } = new URL(req.url);
      if (pathname === "/manifest.json") {
        return new Response(JSON.stringify(manifest), { headers: { "content-type": "application/json" } });
      }
      if (pathname === "/payload") return new Response("malicious-payload-bytes");
      return new Response("not found", { status: 404 });
    },
  });
  manifest.versions[version].assets[0].url = `http://127.0.0.1:${server.port}/payload`;
  return { manifestUrl: `http://127.0.0.1:${server.port}/manifest.json`, stop: () => server.stop(true) };
}

/** A traversal id unique to this run, and the basename it would escape to. */
function escapeId(): { version: string; leak: string } {
  const leak = `lode-pt-${process.pid}-${Date.now()}`;
  return { version: `../../${leak}`, leak };
}

/** Assert nothing named after the traversal id leaked into the data dir's parent. */
function assertNoLeak(dataDir: string, leak: string): void {
  const parent = dirname(dataDir);
  for (const suffix of [".part", ".tmp", ""]) {
    const escaped = join(parent, `${leak}${suffix}`);
    expect(existsSync(escaped)).toBe(false);
  }
}

let h: Harness | undefined;
const cleanups: Array<() => void | Promise<void>> = [];

afterEach(async () => {
  for (const fn of cleanups.splice(0)) await fn();
  await h?.dispose();
  h = undefined;
});

test("lode-cli update refuses a traversal version; current + data dir untouched", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  // Pre-install a legitimate `current` via a bare-lode bootstrap (full sha256 +
  // ed25519 verification), then stop it so the data dir is at rest.
  const boot = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "--readiness", "none"]);
  await boot.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "pre-install -> running v0.0.1",
  });
  await boot.dispose();
  expect(readlinkSync(join(h.dataDir, "current"))).toBe(join("versions", "0.0.1"));
  expect(readdirSync(join(h.dataDir, "versions"))).toEqual(["0.0.1"]);

  // Point lode (same data dir) at a manifest whose channel latest is a traversal
  // id, then resolve that id via an explicit `update --version` — the most direct
  // exercise of the path-traversal validator (resolve_target validates every id it
  // returns, whether followed from `latest` or named explicitly).
  const { version, leak } = escapeId();
  const mal = startMaliciousServer(h.server.name, version);
  cleanups.push(() => mal.stop());
  const malArgs = [
    "--app",
    h.server.name,
    "--data-dir",
    h.dataDir,
    "--manifest",
    mal.manifestUrl,
    "--run",
    "{entry}",
    "--exec",
    "{entry}",
    "--log-level",
    "info",
  ];

  const res = await run([LODE_CLI_BIN, ...malArgs, "update", "--version", version]);

  // (a) refused with a clear error naming the offending id.
  expect(res.exitCode).not.toBe(0);
  expect(`${res.stdout}${res.stderr}`).toMatch(/invalid version/i);

  // (b) no file/dir escaped the data dir.
  assertNoLeak(h.dataDir, leak);

  // (c) the pre-existing current is intact — symlink, versions dir, and state.
  expect(readlinkSync(join(h.dataDir, "current"))).toBe(join("versions", "0.0.1"));
  expect(readdirSync(join(h.dataDir, "versions"))).toEqual(["0.0.1"]);
  const st = JSON.parse(readFileSync(join(h.dataDir, "state.json"), "utf8")) as { current?: string };
  expect(st.current).toBe("0.0.1");
});

test("bare-lode bootstrap refuses a traversal channel-latest (no leak)", async () => {
  const dataDir = mkTmp("lode-pt-data-");
  cleanups.push(() => rmTmp(dataDir));

  const { version, leak } = escapeId();
  const mal = startMaliciousServer(APP_NAME, version);
  cleanups.push(() => mal.stop());

  const lode = new LodeRunner(
    [
      "--app",
      APP_NAME,
      "--data-dir",
      dataDir,
      "--manifest",
      mal.manifestUrl,
      "--run",
      "{entry}",
      "--exec",
      "{entry}",
      "--log-level",
      "info",
      "--policy",
      "off",
      "--readiness",
      "none",
      // An unsigned catalog is fine (catalog signature is verify-if-present), so
      // bootstrap follows the channel `latest` straight into the path-traversal
      // validator. `off` keeps this invocation free of any key wiring.
      "--require-signature",
      "off",
    ],
    dataDir,
    baseEnv(),
  );
  cleanups.push(() => lode.dispose());

  // serve() resolves the (malicious) channel latest during bootstrap; validation
  // makes that resolution fail, so lode exits non-zero without downloading.
  const exit = await lode.waitExit(20000);
  expect(exit.code).not.toBe(0);
  // lode's tracing subscriber writes to stdout; the failure is logged there.
  expect(`${lode.stdout}${lode.stderr}`).toMatch(/invalid version/i);

  assertNoLeak(dataDir, leak);
  // Nothing was installed: no version dir, no current symlink.
  expect(existsSync(join(dataDir, "current"))).toBe(false);
});
