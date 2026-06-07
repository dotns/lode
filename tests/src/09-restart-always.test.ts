// Scenario 10 — restart=always with a persistently-crashing app: lode restarts it
// with exponential backoff up to restart_max, then GIVES UP and exits (status
// error). We assert the launch count is bounded by restart_max+1 and that the
// inter-restart spacing grows (backoff).

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("restart=always: bounded restarts (max) with growing backoff, then give up", async () => {
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

  const exit = await lode.waitExit(20000);
  // Gave up with the child's code (1) after exhausting the restart budget.
  expect(exit.code).toBe(1);

  const starts = lode.matchTimes(/\[app\] starting version=0\.0\.1/);
  // initial launch + restart_max restarts.
  expect(starts.length).toBe(restartMax + 1);

  // Backoff grows between restarts (~200, ~400, ~800 ms).
  const gaps = starts.slice(1).map((t, i) => t - starts[i]);
  expect(gaps.length).toBe(restartMax);
  expect(gaps[0]).toBeGreaterThanOrEqual(120);
  expect(gaps[1]).toBeGreaterThan(gaps[0]);
  expect(gaps[2]).toBeGreaterThan(gaps[1]);

  const st = lode.readState();
  expect(st?.status).toBe("error");
  expect(st?.last_error ?? "").toMatch(/restart limit/i);
});
