// Scenario 3 — update policy=check: lode advertises a newer version in
// state.available but does NOT auto-apply. The test (acting as the app) writes
// state.target=v0.0.2; lode performs the lode-initiated hot-swap and commits it
// (state.current==v0.0.2).

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("policy=check advertises available, then a written target hot-swaps", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "check",
    "--interval",
    "1",
    "--readiness",
    "none",
    "--health-grace",
    "1",
    "--stop-timeout",
    "5",
  ]);

  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1",
  });

  // Publish a newer version; policy=check must advertise it (no auto-apply).
  await h.publish("0.0.2", { mode: "service", latest: true });
  const advertised = await lode.waitForState((s) => s.available === "0.0.2", {
    timeout: 10000,
    label: "available v0.0.2",
  });
  expect(advertised.current).toBe("0.0.1"); // still on the old version

  // App requests the upgrade by writing state.target; lode hot-swaps.
  await lode.requestTarget("0.0.2", (s) => s.status === "updating" || s.current === "0.0.2", {
    timeout: 10000,
    label: "request target v0.0.2",
  });

  const swapped = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 15000,
    label: "swapped to v0.0.2",
  });
  expect(swapped.current).toBe("0.0.2");
  expect(swapped.last_good).toBe("0.0.2");
  await lode.waitForStdout(/\[app\] starting version=0\.0\.2/, { label: "v0.0.2 running" });
  expect(lode.exited).toBe(false);
});
