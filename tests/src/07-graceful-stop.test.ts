// Scenario 7 — graceful stop: SIGTERM to lode => it forwards SIGTERM to the app,
// which cleans up and exits 0 well within stop_timeout (no premature SIGKILL), and
// lode then exits 0 with status=stopped.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("SIGTERM => app cleans up + exits 0 within stop_timeout; lode exits 0", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  const stopTimeout = 5;
  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "off",
    "--readiness",
    "none",
    "--stop-timeout",
    String(stopTimeout),
  ]);

  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1",
  });
  await lode.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "app started" });

  const t0 = Date.now();
  lode.signal("SIGTERM");
  const exit = await lode.waitExit(15000);
  const elapsed = Date.now() - t0;

  // Clean exit (0), not 137 (128+SIGKILL) — proves no premature kill.
  expect(exit.code).toBe(0);
  expect(elapsed).toBeLessThan(stopTimeout * 1000);
  // The app's SIGTERM trap actually ran.
  expect(lode.stdout).toContain("[app] cleanup done, exiting 0");
  expect(lode.readState()?.status).toBe("stopped");
});
