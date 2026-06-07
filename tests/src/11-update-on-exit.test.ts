// Scenario 12 — update-by-app-exit (always active, regardless of restart policy):
// the app writes state.target=v0.0.2 then exit(0). lode must apply the pending
// update and launch v0.0.2 DIRECTLY — no intermediate relaunch of v0.0.1 (no
// old->new flap).

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("app writes target then exit(0) => lode relaunches directly on v0.0.2 (no flap)", async () => {
  h = await Harness.start();
  // v0.0.1 requests v0.0.2 then exits; v0.0.2 is a normal service (present but not latest).
  await h.publish("0.0.1", { mode: "update-on-exit", target: "0.0.2", latest: true });
  await h.publish("0.0.2", { mode: "service", latest: false });

  // restart=off proves the relaunch is the UPDATE path, not a crash-restart.
  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "off",
    "--restart",
    "off",
    "--readiness",
    "none",
    "--health-grace",
    "1",
    "--stop-timeout",
    "5",
  ]);

  const st = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 20000,
    label: "running v0.0.2 after update-on-exit",
  });
  expect(st.current).toBe("0.0.2");
  expect(st.last_good).toBe("0.0.2");

  await lode.waitForStdout(/\[app\] starting version=0\.0\.2/, { label: "v0.0.2 launched" });
  // No flap: v0.0.1 launched exactly once (it never got relaunched as v0.0.1).
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBe(1);
  expect(lode.exited).toBe(false);
});
