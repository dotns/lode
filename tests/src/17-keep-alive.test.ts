// Scenario 17 — keep-alive supervisor (design §8): lode (PID 1) must NOT exit on
// app failure / crash-loop the container. By default it retries a failing app then
// PAUSES (status=error, stays alive). A paused app recovers on (a) a new `target`,
// or (b) an edited `lode.toml`. We cover both — the second proves the config-fix
// recovery the operator reaches for when the failure is in `lode.toml` itself.

import { writeFileSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("default: a crashing app retries then pauses; a new target recovers it", async () => {
  h = await Harness.start();
  // v0.0.1 crashes immediately and forever.
  await h.publish("0.0.1", { mode: "exit", exitCode: 1, latest: true });

  // Default keep-alive (no --restart): retry then pause. Small caps for speed.
  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "off",
    "--restart-max",
    "2",
    "--restart-backoff",
    "1",
    // Small grace: the app crashes instantly (elapsed << grace, so the retry count
    // still accumulates), and the recovered v0.0.2 commits quickly.
    "--health-grace",
    "1",
  ]);

  // It must PAUSE (stay alive), not exit — no container crash-loop.
  const paused = await lode.waitForState(
    (s) => s.status === "error" && (s.last_error ?? "").includes("paused"),
    { timeout: 20000, label: "paused after retries" },
  );
  expect(paused.last_error ?? "").toMatch(/paused/i);
  expect(lode.exited).toBe(false);

  // Recovery via a new target: publish a healthy v0.0.2 and request it.
  await h.publish("0.0.2", { mode: "service", latest: false });
  const recovered = await lode.requestTarget(
    "0.0.2",
    (s) => s.status === "running" && s.current === "0.0.2",
    { timeout: 20000, label: "recovered onto v0.0.2" },
  );
  expect(recovered.current).toBe("0.0.2");
  expect(lode.exited).toBe(false);
});

test("editing lode.toml recovers a paused app (config-fix)", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  // A self-contained lode.toml whose `[command].run` is initially BROKEN, so the
  // app cannot start (spawn failure) → retries → pause. (No baseArgs, so the file
  // is authoritative; asset filename matches the harness's "app.sh".)
  const cfgPath = join(h.dataDir, "lode.toml");
  const config = (run: string) => `
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
  const paused = await lode.waitForState(
    (s) => s.status === "error" && (s.last_error ?? "").includes("paused"),
    { timeout: 20000, label: "paused on broken run command" },
  );
  expect(paused.last_error ?? "").toMatch(/paused/i);
  expect(lode.exited).toBe(false);

  // Operator fixes lode.toml (run -> the real launch command). Sleep first so the
  // rewrite's mtime is strictly newer than the one captured at pause.
  await sleep(1100);
  writeFileSync(cfgPath, config("./app.sh"));

  // lode reloads the fixed config and the app comes up.
  const running = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "recovered after lode.toml fix",
  });
  expect(running.current).toBe("0.0.1");
  expect(lode.exited).toBe(false);
});
