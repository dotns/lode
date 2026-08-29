// Scenario 27 — the runtime cache is keyed by the configured runtime, not just by
// its name. Repointing `[runtime]` at a new version (a new `download` URL, with or
// without a `version` pin) must actually download that runtime: the previous
// download lives under its own `$LODE_DIR/runtime/<key>/` and is reclaimed, so the
// app can never be launched by the stale runtime the old cache held.

import { readdirSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

/** A stand-in runtime: prints `version` for the probe, otherwise announces itself
 *  and runs the app it was handed (so stdout says which runtime launched it). */
function fakeRuntime(version: string): string {
  return `#!/bin/sh
if [ "$1" = "--version" ]; then echo "${version}"; exit 0; fi
echo "[rt] version=${version}"
exec /bin/sh "$@"
`;
}

/** The runtime cache directories present under `$LODE_DIR/runtime/`. Each is named
 *  `<version>-<url digest>` (or `url-<digest>` unpinned), so a test matches by prefix
 *  rather than spelling the digest out. */
function runtimeKeys(dataDir: string): string[] {
  return readdirSync(join(dataDir, "runtime"));
}

/** lode flags for this world, launching the app through the `fakert` runtime. */
function args(h: Harness, download: string, version?: string): string[] {
  return [
    "--app",
    h.server.name,
    "--dir",
    h.dataDir,
    "--manifest",
    h.server.manifestUrl,
    "--asset",
    "app.sh",
    "--run",
    "fakert ./app.sh",
    "--log-level",
    "info",
    ...h.trustArgs("enforce"),
    "--policy",
    "off",
    "--readiness",
    "none",
    "--runtime",
    "fakert",
    "--runtime-download",
    download,
    ...(version ? ["--runtime-version", version] : []),
  ];
}

test("a version-pinned runtime is cached per version and re-downloaded when the pin moves", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });
  const rt1 = h.server.serveFile("rt/1.0.0/fakert", fakeRuntime("1.0.0"));

  // First boot: no runtime on PATH → download it into the cache dir for 1.0.0.
  const lode1 = h.runLodeRaw(args(h, rt1, "1.0.0"));
  await lode1.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "bootstrap -> running v0.0.1 under runtime 1.0.0",
  });
  await lode1.waitForStdout(/\[rt\] version=1\.0\.0/, { label: "app launched by runtime 1.0.0" });
  await lode1.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "app started (run 1)" });
  expect(runtimeKeys(h.dataDir).filter((k) => k.startsWith("1.0.0-"))).toHaveLength(1);
  await lode1.dispose();

  // The operator moves the pin: a new URL AND a new version. The 1.0.0 cache must
  // not satisfy it — lode downloads 2.0.0 and the app runs under the new runtime.
  const rt2 = h.server.serveFile("rt/2.0.0/fakert", fakeRuntime("2.0.0"));
  const lode2 = h.runLodeRaw(args(h, rt2, "2.0.0"));
  await lode2.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "relaunch -> running v0.0.1 under runtime 2.0.0",
  });
  await lode2.waitForStdout(/\[rt\] version=2\.0\.0/, { label: "app launched by runtime 2.0.0" });
  expect(lode2.stdout).not.toMatch(/\[rt\] version=1\.0\.0/);
  // Only the current key survives: the 1.0.0 cache is reclaimed, not left to rot.
  const keys = runtimeKeys(h.dataDir);
  expect(keys.filter((k) => k.startsWith("2.0.0-"))).toHaveLength(1);
  expect(keys.filter((k) => k.startsWith("1.0.0-"))).toHaveLength(0);
});

test("an unpinned runtime is re-downloaded when the download URL changes", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });
  const rt1 = h.server.serveFile("rt/a/fakert", fakeRuntime("1.0.0"));

  // No `--runtime-version`: the cache is keyed by the download URL instead.
  const lode1 = h.runLodeRaw(args(h, rt1));
  await lode1.waitForStdout(/\[rt\] version=1\.0\.0/, { label: "app launched by runtime a" });
  await lode1.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "bootstrap -> running v0.0.1 under runtime a",
  });
  await lode1.dispose();

  // Same runtime name, different URL → a different cache key, so the new bytes are
  // fetched rather than the old ones reused.
  const rt2 = h.server.serveFile("rt/b/fakert", fakeRuntime("2.0.0"));
  const lode2 = h.runLodeRaw(args(h, rt2));
  await lode2.waitForStdout(/\[rt\] version=2\.0\.0/, { label: "app launched by runtime b" });
  expect(lode2.stdout).not.toMatch(/\[rt\] version=1\.0\.0/);
});
