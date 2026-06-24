// Scenario 25 (design §7) — a `lode.toml` edit while the app is RUNNING must NOT
// auto-restart the app. lode only NOTIFIES the app by bumping `state.config_generation`;
// the app applies the change at its own pace by bumping `restart_nonce`, which makes
// lode RE-READ lode.toml and relaunch with the new config (here: a new `[env]` value).

import { writeFileSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("running lode.toml edit notifies the app (config_generation) without restarting; nonce reload applies it", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  const cfgPath = join(h.dataDir, "lode.toml");
  // The app echoes the `[env].RELOAD_MARKER` value in its startup log, so we can
  // observe whether a relaunch picked up the edited config.
  const config = (marker: string) => `
[global]
app = "${h.server.name}"
dir = "${h.dataDir}"

[update]
manifest = "${h.server.manifestUrl}"
asset = "app.sh"
policy = "off"

[trust]
require_signature = "enforce"
trusted_keys = ["${h.trustedKey}"]

[command]
run = "./app.sh"
exec = "./app.sh"

[env]
RELOAD_MARKER = "${marker}"

[supervise]
restart = "on-failure"
restart_max = 2
restart_backoff = 1
health_grace = 60
`;

  writeFileSync(cfgPath, config("one"));
  const lode = h.runLodeRaw(["--config", cfgPath, "--log-level", "info"]);

  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "app running with initial config",
  });
  // Started once, with the initial env marker; no config generation yet.
  expect(lode.countMatches(/\[app\] starting .*marker=one/)).toBe(1);
  // lode injects LODE_CONFIG = the resolved lode.toml path (read-only for the app).
  expect(lode.countMatches(new RegExp(`config=${cfgPath.replace(/[.*+?^${}()|[\]\\/]/g, "\\$&")}\\b`))).toBe(1);
  expect(lode.readState()?.config_generation ?? 0).toBe(0);

  // --- B: edit lode.toml while the app RUNS -> notify only, never auto-restart ---
  await sleep(1100); // ensure the rewrite's mtime is strictly newer
  writeFileSync(cfgPath, config("two"));

  const notified = await lode.waitForState((s) => (s.config_generation ?? 0) >= 1, {
    timeout: 20000,
    label: "config_generation bumped on a running edit",
  });
  expect(notified.config_generation).toBe(1);
  // The app was NOT restarted: still exactly one start, still the OLD marker.
  expect(lode.countMatches(/\[app\] starting/)).toBe(1);
  expect(lode.countMatches(/marker=two/)).toBe(0);
  expect(lode.exited).toBe(false);

  // --- A: the app applies it at its own pace by bumping restart_nonce -> reload ---
  const nonce = (lode.readState()?.restart_nonce ?? 0) + 1;
  lode.writeStateField("restart_nonce", nonce);

  // The relaunch must re-read lode.toml and carry the NEW env marker.
  for (let i = 0; i < 200 && lode.countMatches(/\[app\] starting/) < 2; i++) {
    await sleep(100);
  }
  expect(lode.countMatches(/\[app\] starting/)).toBe(2);
  expect(lode.countMatches(/marker=two/)).toBe(1);
  // config_generation is unchanged by the relaunch (no spurious re-notify).
  await lode.waitForState((s) => s.status === "running", {
    timeout: 20000,
    label: "running again after nonce reload",
  });
  expect(lode.readState()?.config_generation ?? 0).toBe(1);
  expect(lode.exited).toBe(false);
});
