// Scenario 15 — download cache: a verified artifact is kept under
// `downloads/<ver>/<asset>` after extraction, and download is decoupled from launch.
// We bootstrap a version, drop the server's artifact bytes (so any re-download 404s)
// and delete the extracted tree, then restart: lode must rebuild `versions/<ver>`
// from its local cache and run again WITHOUT re-fetching — proving "only download
// when the cache is absent".

import { existsSync, rmSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("verified download is cached and reused on relaunch without re-fetching", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  // First boot: download + verify (enforce → ed25519) + install + run.
  const lode1 = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "--readiness", "none"]);
  await lode1.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "bootstrap -> running v0.0.1",
  });
  await lode1.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "app started (run 1)" });

  // The verified artifact is retained as a per-version cache (not deleted on extract).
  const cached = join(h.dataDir, "downloads", "0.0.1", "app.sh");
  expect(existsSync(cached)).toBe(true);

  // Stop the supervisor, then make a fresh download impossible: drop the server's
  // artifact bytes (manifest stays served) and delete the extracted version so the
  // next boot must re-install.
  await lode1.dispose();
  h.server.dropArtifact("0.0.1");
  rmSync(join(h.dataDir, "versions", "0.0.1"), { recursive: true, force: true });
  expect(existsSync(cached)).toBe(true); // cache survives the version deletion

  // Second boot: re-extracts from the cache and runs again despite the artifact 404 —
  // it never touched the network for the body.
  const lode2 = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "--readiness", "none"]);
  await lode2.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "relaunch from cache -> running v0.0.1",
  });
  await lode2.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "app started (run 2)" });
  expect(lode2.exited).toBe(false);
});
