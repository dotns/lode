// Scenario 11 — restart=on-failure: a clean exit(0) makes lode exit too (no
// restart), while a crash triggers bounded-backoff restarts (giving up at
// restart_max).

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("restart=on-failure: app exit(0) => lode exits 0, no relaunch", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "exit", exitCode: 0, latest: true });

  const lode = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "--restart", "on-failure"]);

  const exit = await lode.waitExit(20000);
  expect(exit.code).toBe(0);
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBe(1);
  expect(lode.readState()?.status).toBe("stopped");
});

test("restart=on-failure: app crash => lode restarts (bounded by restart_max)", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "exit", exitCode: 5, latest: true });

  const restartMax = 2;
  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "off",
    "--restart",
    "on-failure",
    "--restart-max",
    String(restartMax),
    "--restart-backoff",
    "200",
    "--restart-backoff-max",
    "5000",
    "--health-grace",
    "60",
  ]);

  const exit = await lode.waitExit(20000);
  expect(exit.code).toBe(5);
  // initial launch + restart_max restarts => it DID restart on failure.
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBe(restartMax + 1);
  const st = lode.readState();
  expect(st?.status).toBe("error");
  expect(st?.last_error ?? "").toMatch(/restart limit/i);
});
