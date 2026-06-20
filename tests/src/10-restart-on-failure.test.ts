// Scenario 11 — restart=on-failure: a clean exit(0) makes lode exit too (no
// restart), while a crash triggers bounded-backoff restarts, then PAUSES
// (keep-alive: lode stays alive at restart_max rather than exiting).

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

test("restart=on-failure: app crash => bounded restarts then pause (stay alive)", async () => {
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
    "1",
    "--restart-backoff-max",
    "5",
    "--health-grace",
    "60",
  ]);

  // After the bounded retries lode PAUSES — it must NOT exit (PID 1 stays alive).
  const paused = await lode.waitForState(
    (s) => s.status === "error" && (s.last_error ?? "").includes("paused"),
    { timeout: 20000, label: "paused after bounded retries" },
  );
  expect(paused.last_error ?? "").toMatch(/paused/i);
  // initial launch + restart_max retries => it DID restart on failure, then paused.
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBe(restartMax + 1);
  expect(lode.exited).toBe(false);
});
