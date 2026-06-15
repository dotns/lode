// Scenario 19 — the documented `target:"latest"` alias (integration contract §2):
// an app requests an update by writing {"target":"latest"} into state.json. lode
// must re-resolve the alias through the channel pointer — on the hot-update path
// (running app) AND on the update-on-exit path — instead of passing "latest" to an
// exact-version lookup (which never matches, dropping the request on the running
// path and routing a cleanly-exiting app into pause on the exit path).

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test('running app writes target:"latest" => lode hot-updates to the channel latest', async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

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

  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1",
  });

  // A newer latest appears; the app asks for it by alias, not by version.
  await h.publish("0.0.2", { mode: "service", latest: true });
  await lode.requestTarget("latest", (s) => s.current === "0.0.2", {
    timeout: 20000,
    label: "latest alias applied",
  });

  const st = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 20000,
    label: "running v0.0.2 after latest-alias hot-update",
  });
  expect(st.current).toBe("0.0.2");
  expect(st.last_good).toBe("0.0.2");

  // The request was consumed, lode is still alive on v0.0.2, and v0.0.1 ran
  // exactly once (stopped for the update — never relaunched or crash-restarted).
  await sleep(500);
  expect(lode.exited).toBe(false);
  expect(lode.readState()?.target ?? null).toBe(null);
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBe(1);
});

test('app writes target:"latest" then exit(0) => update-on-exit resolves the alias (no pause)', async () => {
  h = await Harness.start();
  // v0.0.1 requests "latest" then exits cleanly; v0.0.2 is the channel latest.
  // The pin makes bootstrap start on v0.0.1 even though the channel already
  // points at v0.0.2 (an explicit "latest" request re-resolves past the pin).
  await h.publish("0.0.1", { mode: "update-on-exit", target: "latest", latest: true });
  await h.publish("0.0.2", { mode: "service", latest: true });

  // restart=on-failure mirrors the bug being regressed: an unresolvable "latest"
  // used to route this healthy, cleanly-exiting app into pause.
  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--pin",
    "0.0.1",
    "--policy",
    "off",
    "--restart",
    "on-failure",
    "--readiness",
    "none",
    "--health-grace",
    "1",
    "--stop-timeout",
    "5",
  ]);

  const st = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 20000,
    label: "running v0.0.2 after latest-alias update-on-exit",
  });
  expect(st.current).toBe("0.0.2");

  await lode.waitForStdout(/\[app\] starting version=0\.0\.2/, { label: "v0.0.2 launched" });
  // No pause, no flap: v0.0.1 launched exactly once and lode stayed alive.
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBe(1);
  expect(lode.exited).toBe(false);
});
