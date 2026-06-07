// Scenarios 8 & 9 — restart=off (the default): lode MIRRORS the child's lifecycle.
// On a non-lode-initiated child exit with no pending update, lode exits with the
// child's code and does NOT relaunch.
//   8: app exit(0)        => lode exits 0, status=stopped, exactly one launch.
//   9: app crash exit(7)  => lode exits 7, status=error, exactly one launch.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("restart=off: app exit(0), no update => lode exits 0, no relaunch", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "exit", exitCode: 0, latest: true });

  const lode = h.runLode([...h.trustArgs("enforce"), "--restart", "off", "--policy", "off"]);

  const exit = await lode.waitExit(20000);
  expect(exit.code).toBe(0);
  // No relaunch: the app was launched exactly once.
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBe(1);
  expect(lode.readState()?.status).toBe("stopped");
});

test("restart=off: app crash exit(7), no update => lode exits 7, no relaunch", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "exit", exitCode: 7, latest: true });

  const lode = h.runLode([...h.trustArgs("enforce"), "--restart", "off", "--policy", "off"]);

  const exit = await lode.waitExit(20000);
  expect(exit.code).toBe(7);
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBe(1);
  const st = lode.readState();
  expect(st?.status).toBe("error");
  expect(st?.last_error ?? "").toMatch(/7/);
});
