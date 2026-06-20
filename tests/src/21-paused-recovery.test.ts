// Scenario 21 — paused-recovery apply failure must not strand lode in limbo
// (P2-12): a paused app's recovery `target` naming a version that cannot be
// installed (here: never published) must leave lode PAUSED (status=error, alive),
// with the request consumed — and the documented lode.toml-edit recovery must
// still work afterwards. Before the fix, the failed apply cleared the pause
// without producing a child, and config_changed() (gated on `paused`) went dead.

import { writeFileSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("an uninstallable recovery target keeps lode paused; lode.toml edit still recovers", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  // A self-contained lode.toml whose `[command].run` is initially BROKEN, so the
  // app cannot start (spawn failure) → retries → pause (same shape as scenario 17).
  const cfgPath = join(h.dataDir, "lode.toml");
  const config = (run: string) => `
[global]
app = "${h.server.name}"
data_dir = "${h.dataDir}"

[update]
manifest = "${h.server.manifestUrl}"
asset = "app.sh"
policy = "off"

[trust]
require_signature = "enforce"
trusted_keys = ["${h.trustedKey}"]

[command]
run = "${run}"
exec = "./app.sh"

[supervise]
restart = "on-failure"
restart_max = 2
restart_backoff = 1
health_grace = 60
`;

  writeFileSync(cfgPath, config("/nonexistent/lode-e2e-binary"));
  const lode = h.runLodeRaw(["--config", cfgPath, "--log-level", "info"]);

  // The broken run command can't start the app → lode pauses (stays alive).
  await lode.waitForState((s) => s.status === "error" && (s.last_error ?? "").includes("paused"), {
    timeout: 20000,
    label: "paused on broken run command",
  });
  expect(lode.exited).toBe(false);

  // Recovery attempt via a target that CANNOT be installed (never published).
  // lode must consume the request and STAY paused — not go limbo (un-paused,
  // childless, deaf to lode.toml edits).
  lode.writeStateField("target", "9.9.9");
  const stillPaused = await lode.waitForState(
    (s) =>
      s.status === "error" &&
      s.target === undefined &&
      (s.last_error ?? "").includes("could not be applied"),
    { timeout: 20000, label: "stayed paused after failed recovery apply" },
  );
  expect(stillPaused.last_error ?? "").toMatch(/9\.9\.9/);
  expect(lode.exited).toBe(false);

  // The documented config-fix recovery must STILL work: fix the run command and
  // lode reloads + comes up. Sleep first so the rewrite's mtime is strictly newer.
  await sleep(1100);
  writeFileSync(cfgPath, config("./app.sh"));

  const running = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "recovered after lode.toml fix",
  });
  expect(running.current).toBe("0.0.1");
  expect(lode.exited).toBe(false);
});
