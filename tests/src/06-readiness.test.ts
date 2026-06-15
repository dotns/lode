// Scenario 6 — readiness=state handshake: on a lode-initiated update, lode must
// wait for the new version to write state.ready == its LODE_INSTANCE before
// committing it (status running, last_good=new). We gate the new version so it
// stays un-ready, observe lode WAITING (status=updating, last_good still old),
// then open the gate and observe the commit.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("readiness=state: lode waits for the ready handshake before committing", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "off",
    "--readiness",
    "state",
    "--ready-timeout",
    "25",
    "--health-grace",
    "1",
    "--stop-timeout",
    "5",
  ]);

  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1",
  });

  // v0.0.2 is gated: it will NOT announce readiness until we drop the gate file.
  await h.publish("0.0.2", { mode: "service", latest: false, gate: true });

  // Request the upgrade; once lode begins observing v0.0.2 we stop re-asserting.
  await lode.requestTarget("0.0.2", (s) => s.status === "updating" || s.current === "0.0.2", {
    timeout: 10000,
    label: "begin observing v0.0.2",
  });
  await lode.waitForStdout(/\[app\] starting version=0\.0\.2/, { label: "v0.0.2 spawned" });

  // While un-ready, lode must NOT commit: status stays updating, last_good still v1.
  await sleep(1500);
  const waiting = lode.readState();
  expect(waiting?.status).toBe("updating");
  expect(waiting?.current).toBe("0.0.2");
  expect(waiting?.last_good).toBe("0.0.1");

  // Drop the gate => app writes state.ready=$LODE_INSTANCE-0 (serving) => lode commits.
  h.openReadinessGate();

  const committed = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 15000,
    label: "committed v0.0.2 after ready",
  });
  expect(committed.last_good).toBe("0.0.2");
  expect(lode.exited).toBe(false);
});
