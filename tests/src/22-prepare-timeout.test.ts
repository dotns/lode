// Scenario 22 — [supervise].prepare_timeout (P2-13): a staged update is normally
// app-paced (no timeout — scenario 16), but an app that NEVER acks the "-1"
// prepare prompt must not wedge the update forever. With prepare_timeout=1 lode
// force-cuts-over after a second: the old child is stopped, the staged version
// spawns, readies, and commits. The knob is TOML-only, so this drives lode from a
// lode.toml via --config.

import { writeFileSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("prepare_timeout forces the cut-over when the app never acks the prepare prompt", async () => {
  h = await Harness.start();
  // v0.0.1 gates its prepare ack on $LODE_DIR/prepare_ok — which this test
  // never creates, so it never acks. Its serving (-0) is immediate.
  await h.publish("0.0.1", { mode: "service", latest: true, preGate: true });

  const cfgPath = join(h.dataDir, "lode.toml");
  writeFileSync(
    cfgPath,
    `
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

[supervise]
readiness = "state"
ready_timeout = 25
prepare_timeout = 1
health_grace = 1
stop_timeout = 5
`,
  );
  const lode = h.runLodeRaw(["--config", cfgPath, "--log-level", "info"]);

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

  // The app never acks (-2). After prepare_timeout=1s lode must force the
  // cut-over anyway: v0.0.2 spawns, readies, and commits.
  await lode.waitForStdout(/\[app\] starting version=0\.0\.2/, {
    label: "v0.0.2 spawned after forced cut-over",
  });
  const committed = await lode.waitForState(
    (s) => s.status === "running" && s.current === "0.0.2",
    { timeout: 15000, label: "committed v0.0.2 after forced cut-over" },
  );
  expect(committed.last_good).toBe("0.0.2");
  expect(`${lode.stdout}${lode.stderr}`).toMatch(/forcing cut-over/);
  expect(lode.exited).toBe(false);
});
