// Scenario 14 — single-strike rollback: a freshly-activated version that crashes
// within health_grace is rolled back to last_good after ONE strike (no threshold).
// state.current returns to the prior good version, with a `bad` history entry for
// the failed version and the rollback target committed `good`.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("bad new version crashing within health_grace => single-strike rollback to last_good", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  // policy=off so there is no auto re-apply loop after we roll back.
  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "off",
    "--readiness",
    "none",
    "--health-grace",
    "1",
    "--ready-timeout",
    "30",
    "--stop-timeout",
    "5",
  ]);

  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running good v0.0.1",
  });

  // v0.0.2 crashes immediately (well within health_grace) once activated.
  await h.publish("0.0.2", { mode: "exit", exitCode: 1, latest: false });

  // Request the update; stop re-asserting target as soon as v0.0.2 has launched,
  // so we don't re-trigger an update after the rollback completes.
  await lode.requestTarget(
    "0.0.2",
    (s) => s.status === "updating" || s.status === "rolling-back" || lode.countMatches(/\[app\] starting version=0\.0\.2/) > 0,
    { timeout: 15000, label: "begin applying bad v0.0.2" },
  );

  // Single strike => roll back to v0.0.1 and re-commit it as good.
  const rolled = await lode.waitForState(
    (s) =>
      s.status === "running" &&
      s.current === "0.0.1" &&
      (s.history ?? []).some((hh) => hh.version === "0.0.2" && hh.result === "bad"),
    { timeout: 20000, label: "rolled back to v0.0.1" },
  );

  expect(rolled.current).toBe("0.0.1");
  expect(rolled.last_good).toBe("0.0.1");
  expect((rolled.history ?? []).some((hh) => hh.version === "0.0.2" && hh.result === "bad")).toBe(true);
  // The bad version did launch (proving it was activated) before the rollback.
  expect(lode.countMatches(/\[app\] starting version=0\.0\.2/)).toBeGreaterThanOrEqual(1);
  expect(lode.exited).toBe(false);
});
