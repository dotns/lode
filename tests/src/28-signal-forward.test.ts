// Scenario 28 (design §8) — signal passthrough. lode is the master process: the
// `[signals].forward` set is relayed to the child AS-IS (default set: SIGHUP,
// SIGUSR1, SIGUSR2, SIGWINCH, SIGCONT, SIGTSTP), a signal outside that set is
// ignored (never reaches the app), and an opt-in `[signals].restart` signal is
// CONSUMED by lode as a graceful-restart request instead of being forwarded. The
// fixture app logs every forwarded signal it traps, so each path is observable.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

const SERVICE_ARGS = ["--policy", "off", "--readiness", "none", "--health-grace", "1", "--stop-timeout", "5"];

test("default forward set: SIGHUP / SIGUSR1 / SIGUSR2 / SIGWINCH sent to lode reach the app", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service" });
  const lode = h.runLode([...h.trustArgs("enforce"), ...SERVICE_ARGS]);
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", { timeout: 20000, label: "running" });
  await lode.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "app up" });

  for (const [sig, name] of [
    ["SIGHUP", "HUP"],
    ["SIGUSR1", "USR1"],
    ["SIGUSR2", "USR2"],
    ["SIGWINCH", "WINCH"],
  ] as const) {
    lode.signal(sig);
    await lode.waitForStdout(new RegExp(`\\[app\\] signal ${name} received`), { timeout: 5000, label: `${sig} forwarded` });
  }
  // Passthrough never restarts or stops anything: one app start, lode still up.
  expect(lode.countMatches(/\[app\] starting/)).toBe(1);
  expect(lode.readState()?.status).toBe("running");
  expect(lode.exited).toBe(false);
});

test("--forward-signals narrows the set: a listed signal is forwarded, an unlisted one never reaches the app", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service" });
  const lode = h.runLode([...h.trustArgs("enforce"), ...SERVICE_ARGS, "--forward-signals", "SIGUSR1"]);
  await lode.waitForState((s) => s.status === "running", { timeout: 20000, label: "running" });
  await lode.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "app up" });

  // SIGWINCH is outside the configured set: lode does not register it (default
  // disposition: ignored), so nothing reaches the app and nothing dies.
  lode.signal("SIGWINCH");
  await sleep(1500);
  expect(lode.countMatches(/\[app\] signal WINCH received/)).toBe(0);
  expect(lode.exited).toBe(false);

  lode.signal("SIGUSR1");
  await lode.waitForStdout(/\[app\] signal USR1 received/, { timeout: 5000, label: "SIGUSR1 forwarded" });
  expect(lode.readState()?.status).toBe("running");
});

test("--restart-signal: the signal is consumed as a graceful restart, not forwarded", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service" });
  const lode = h.runLode([...h.trustArgs("enforce"), ...SERVICE_ARGS, "--restart-signal", "SIGUSR2"]);
  await lode.waitForState((s) => s.status === "running", { timeout: 20000, label: "running" });
  await lode.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "app up" });
  const firstPid = lode.readState()?.pid;

  lode.signal("SIGUSR2");
  // Graceful restart: the app is SIGTERMed (it cleans up and exits 0) and the
  // same version is relaunched under a new pid.
  await lode.waitForStdout(/\[app\] SIGTERM received/, { timeout: 10000, label: "old child stopped" });
  await lode.waitForState((s) => s.status === "running" && s.pid !== undefined && s.pid !== firstPid, {
    timeout: 20000,
    label: "relaunched under a new pid",
  });
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBe(2);
  // Consumed, not forwarded: the app never saw SIGUSR2 itself.
  expect(lode.countMatches(/\[app\] signal USR2 received/)).toBe(0);
  expect(lode.readState()?.current).toBe("0.0.1");
  expect(lode.exited).toBe(false);
});
