// Scenario 16 — staged-update prepare handshake (readiness=state): the app paces
// the cut-over. On an update request lode stages+installs the target and prompts
// the RUNNING app with state.ready="$LODE_INSTANCE-1" instead of swapping. lode must
// NOT cut over (current stays old, no new spawn) until the app acks "prepared" with
// "-2". We gate the ack, observe lode WAITING, then drop the gate and see the
// cut-over + commit. Complements scenario 6, which gates the post-cut-over readiness.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("readiness=state: lode waits for the prepare ack before cutting over", async () => {
  h = await Harness.start();
  // v0.0.1 defers its prepare ack (-2) until we drop prepare_ok — so it holds the
  // cut-over while it "prepares". Its own serving (-0) is immediate.
  await h.publish("0.0.1", { mode: "service", latest: true, preGate: true });

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

  // v0.0.2 readies on its own once cut over (no gate).
  await h.publish("0.0.2", { mode: "service", latest: false });

  // Request the upgrade; lode stages it and prompts the running app to prepare.
  await lode.requestTarget("0.0.2", (s) => s.status === "updating", {
    timeout: 10000,
    label: "staged + prompting prepare",
  });

  // Cut-over is gated on the app's ack: lode must keep v0.0.1 running with the "-1"
  // prompt outstanding, and must NOT have spawned v0.0.2 yet.
  await sleep(2000);
  const waiting = lode.readState();
  expect(waiting?.status).toBe("updating");
  expect(waiting?.current).toBe("0.0.1");
  expect(waiting?.ready?.endsWith("-1")).toBe(true);
  expect(lode.countMatches(/\[app\] starting version=0\.0\.2/)).toBe(0);

  // Drop the prepare gate => v0.0.1 acks "-2" => lode cuts over => v0.0.2 readies => commit.
  h.openPrepareGate();
  await lode.waitForStdout(/\[app\] starting version=0\.0\.2/, { label: "v0.0.2 spawned after ack" });

  const committed = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 15000,
    label: "committed v0.0.2 after cut-over",
  });
  expect(committed.last_good).toBe("0.0.2");
  expect(lode.exited).toBe(false);
});
