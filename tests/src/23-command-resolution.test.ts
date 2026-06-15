// Scenario 23 — launch-command resolution (entry abolition): the launch command
// is the manifest asset's signed `run` override, else `[command].run`, else a
// clear hard error. (a) an asset publishing `run` launches via it with NO
// [command] section in lode.toml at all; (b) [command].run with no manifest
// override is every other scenario in this suite; (c) when neither side supplies
// a command, lode reports "no run command", pauses (keep-alive), and stays up.

import { writeFileSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

/** A self-contained lode.toml WITHOUT any [command] section (the file is
 *  authoritative — no baseArgs), so the launch command can only come from the
 *  manifest asset's published override. */
function configWithoutCommand(extraSupervise = ""): string {
  return `
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

[supervise]
readiness = "none"
health_grace = 1
${extraSupervise}
`;
}

test("a manifest `run` override launches the app without any [command] in lode.toml", async () => {
  h = await Harness.start();
  // The publisher signs AND publishes run = "./app.sh" (the raw artifact lands
  // under its asset filename, and cwd is the version dir).
  await h.publish("0.0.1", { mode: "service", latest: true, run: "./app.sh" });

  const cfgPath = join(h.dataDir, "lode.toml");
  writeFileSync(cfgPath, configWithoutCommand());
  const lode = h.runLodeRaw(["--config", cfgPath, "--log-level", "info"]);

  // Bootstrap + launch purely via the manifest override.
  const st = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1 via manifest run override",
  });
  expect(st.current).toBe("0.0.1");
  await lode.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "app banner" });
  expect(lode.exited).toBe(false);
});

test("missing both [command].run and manifest run => clear 'no run command' error, lode stays alive", async () => {
  h = await Harness.start();
  // No `run` published with the asset…
  await h.publish("0.0.1", { mode: "service", latest: true });

  // …and no [command] in lode.toml either. Small retry caps for speed.
  const cfgPath = join(h.dataDir, "lode.toml");
  writeFileSync(cfgPath, configWithoutCommand("restart = \"on-failure\"\nrestart_max = 1\nrestart_backoff = 100\n"));
  const lode = h.runLodeRaw(["--config", cfgPath, "--log-level", "info"]);

  // The spawn fails with the actionable resolution error…
  await lode.waitForStdout(/no run command: set \[command\]\.run or publish `run` in the manifest asset/, {
    timeout: 20000,
    label: "'no run command' surfaced",
  });
  // …and the keep-alive supervisor pauses instead of exiting (PID-1 contract).
  const paused = await lode.waitForState(
    (s) => s.status === "error" && (s.last_error ?? "").includes("paused"),
    { timeout: 20000, label: "paused after retries" },
  );
  expect(paused.last_error ?? "").toMatch(/paused/i);
  expect(lode.exited).toBe(false);
});
