// Scenario 26 (design §7) — app-requested HOLD. The app (or an operator) can ask
// lode NOT to (re)start the process by setting `state.json`'s `hold` flag — for
// planned maintenance that must finish before the app comes up (e.g. a DB
// migration needing CLI intervention). lode reports status `held` and waits; it
// gates a *start*, not a running child. Clearing `hold` resumes a normal start.

import { writeFileSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("hold present at boot: lode installs the version but does NOT start it (status=held); release starts it", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  // The operator pre-sets the hold flag before lode boots (the file survives
  // bootstrap's read-modify-writes, which preserve app-owned fields).
  writeFileSync(join(h.dataDir, "state.json"), `${JSON.stringify({ hold: true }, null, 2)}\n`);

  const lode = h.runLode([...h.trustArgs("enforce"), "--policy", "off"]);

  // lode bootstraps + installs 0.0.1 but must NOT spawn the app — it holds.
  // (`current` is written at spawn, so it stays unset while held — nothing runs.)
  const held = await lode.waitForState((s) => s.status === "held", {
    timeout: 20000,
    label: "held at boot (not started)",
  });
  expect(held.pid ?? null).toBeNull(); // no child is running
  expect(lode.countMatches(/\[app\] starting/)).toBe(0); // the app never started
  expect(lode.exited).toBe(false); // PID 1 stays alive, waiting

  // Stays held while the flag is set (no auto-start sneaks in).
  await sleep(2000);
  expect(lode.countMatches(/\[app\] starting/)).toBe(0);
  expect(lode.readState()?.status).toBe("held");

  // Releasing the hold resumes a normal start.
  lode.writeStateField("hold", false);
  const running = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "started after release",
  });
  expect(running.current).toBe("0.0.1");
  expect(lode.countMatches(/\[app\] starting/)).toBe(1);
  expect(lode.exited).toBe(false);
});

test("hold defers a restart_nonce request and never disturbs the running child", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  const lode = h.runLode([...h.trustArgs("enforce"), "--policy", "off"]);
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "app running",
  });
  expect(lode.countMatches(/\[app\] starting/)).toBe(1);

  // Hold, then request a restart: while held the nonce is deferred — the running
  // child is left untouched (hold gates starts, not a running process).
  lode.writeStateField("hold", true);
  await sleep(1500); // let lode observe the hold
  const nonce = (lode.readState()?.restart_nonce ?? 0) + 1;
  lode.writeStateField("restart_nonce", nonce);

  // Give lode ample time to (not) act on the deferred nonce.
  await sleep(3000);
  expect(lode.countMatches(/\[app\] starting/)).toBe(1); // NOT restarted
  expect(lode.exited).toBe(false);

  // Releasing the hold does not respawn either (the child was never stopped).
  lode.writeStateField("hold", false);
  await sleep(2000);
  expect(lode.countMatches(/\[app\] starting/)).toBe(1);
  expect(lode.readState()?.status).toBe("running");
});
