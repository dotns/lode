// Scenario 13 — auto-update of a RUNNING app (lode-initiated, always active): with
// restart=off and policy=auto, a newer latest makes lode stop the old child and
// launch the new one as an UPDATE (not a crash). Crucially, lode keeps running on
// v0.0.2 — it does NOT mistake the lode-initiated stop for a child exit that would
// (under restart=off) terminate lode.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("policy=auto + restart=off: running app is hot-updated; lode stays alive on v0.0.2", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "auto",
    "--interval",
    "1",
    "--restart",
    "off",
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

  await h.publish("0.0.2", { mode: "service", latest: true });

  const st = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 20000,
    label: "hot-updated to v0.0.2",
  });
  expect(st.current).toBe("0.0.2");
  expect(st.last_good).toBe("0.0.2");

  // The lode-initiated stop of v0.0.1 was treated as an update, NOT a crash:
  // lode is still alive and supervising v0.0.2 a moment later.
  await sleep(500);
  expect(lode.exited).toBe(false);
  expect(lode.readState()?.current).toBe("0.0.2");
  // v0.0.1 ran exactly once (stopped for the update, never crash-restarted).
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBe(1);
});
