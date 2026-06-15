// Scenario 10 — restart=always with a persistently-crashing app: lode restarts it
// with exponential backoff up to restart_max, then PAUSES (keep-alive) — it stays
// alive (PID 1 must not crash-loop the container), status=error. We assert the
// launch count is bounded by restart_max+1, the inter-restart spacing grows
// (backoff), and lode does NOT exit.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("restart=always: bounded restarts (max) with growing backoff, then pause (stay alive)", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "exit", exitCode: 1, latest: true });

  const restartMax = 3;
  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "off",
    "--restart",
    "always",
    "--restart-max",
    String(restartMax),
    "--restart-backoff",
    "200",
    "--restart-backoff-max",
    "5000",
    // Large grace so a fast crash never resets the consecutive-restart counter.
    "--health-grace",
    "60",
  ]);

  // After exhausting the retry budget lode PAUSES (does not exit).
  const paused = await lode.waitForState(
    (s) => s.status === "error" && (s.last_error ?? "").includes("paused"),
    { timeout: 20000, label: "paused after bounded retries" },
  );
  expect(paused.last_error ?? "").toMatch(/paused/i);

  const starts = lode.matchTimes(/\[app\] starting version=0\.0\.1/);
  // initial launch + restart_max retries, then it stops retrying (pause).
  expect(starts.length).toBe(restartMax + 1);

  // Backoff grows between restarts (~200, ~400, ~800 ms).
  const gaps = starts.slice(1).map((t, i) => t - starts[i]);
  expect(gaps.length).toBe(restartMax);
  expect(gaps[0]).toBeGreaterThanOrEqual(120);
  expect(gaps[1]).toBeGreaterThan(gaps[0]);
  expect(gaps[2]).toBeGreaterThan(gaps[1]);

  // PID 1 stays alive (no container crash-loop).
  expect(lode.exited).toBe(false);
});
