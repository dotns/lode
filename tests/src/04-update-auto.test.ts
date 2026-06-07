// Scenario 4 — update policy=auto: when the manifest's channel-latest advances,
// lode auto-applies (sets its own target, hot-swaps, observes, commits) with no
// app/test involvement. state.current ends at v0.0.2.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("policy=auto auto-applies a newer channel-latest", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "auto",
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

  // Advance the channel; no target written by us — lode must apply on its own.
  await h.publish("0.0.2", { mode: "service", latest: true });

  const swapped = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 20000,
    label: "auto-applied v0.0.2",
  });
  expect(swapped.current).toBe("0.0.2");
  expect(swapped.last_good).toBe("0.0.2");
  await lode.waitForStdout(/\[app\] starting version=0\.0\.2/, { label: "v0.0.2 running" });
  expect(lode.exited).toBe(false);
});
