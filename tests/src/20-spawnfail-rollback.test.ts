// Scenario 20 — PID-1 hardening (P1-6): a freshly-activated version whose launch
// target exists but CANNOT BE EXEC'D (bad interpreter / lost exec bit / wrong-arch
// binary) must roll back to last_good — with the old child already stopped at
// spawn time, lode (PID 1) must not exit. Unlike scenario 13 (crash AFTER a
// successful spawn), the failure here happens at spawn itself, which used to
// `?`-propagate out of apply_target and kill the supervisor.

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { mkTmp, rmTmp } from "./helpers/util.ts";

let h: Harness;
let scratch: string | undefined;

afterEach(async () => {
  await h?.dispose();
  if (scratch) rmTmp(scratch);
  scratch = undefined;
});

/** Publish a version whose artifact passes install (sha256 + signature are valid,
 *  the launch target lands in place) but fails at exec time: its shebang names an
 *  interpreter that does not exist, so spawning it yields ENOENT. install
 *  `chmod +x`es the run command's target, so a bad interpreter — not a missing
 *  exec bit — is the reliable "garbage that fails exec" artifact. */
async function publishUnexecutable(version: string): Promise<void> {
  scratch ??= mkTmp("lode-garbage-");
  const dir = join(scratch, version);
  mkdirSync(dir, { recursive: true });
  // The asset filename must be "app.sh" — the harness's selection key
  // ([update].asset), the basename the signature binds, and where the raw
  // artifact lands (so the harness's run "./app.sh" targets it).
  const artifactPath = join(dir, "app.sh");
  writeFileSync(artifactPath, "#!/nonexistent/lode-e2e-interpreter\necho unreachable\n");
  const signed = await h.signer.sign(artifactPath, version);
  h.server.publish(version, {
    artifactPath,
    name: "app.sh",
    sha256: signed.sha256,
    sig: signed.sig,
    keyId: h.signer.keyId,
    latest: false,
  });
}

test("update target that fails to spawn => rollback to last_good, lode stays alive", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  // policy=off so there is no auto re-apply loop after we roll back.
  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "off",
    "--readiness",
    "none",
    "--health-grace",
    "1",
    "--ready-timeout",
    "30",
    "--stop-timeout",
    "5",
  ]);

  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running good v0.0.1",
  });

  // v0.0.2 installs fine but its launch target cannot be exec'd.
  await publishUnexecutable("0.0.2");

  // Request the update; stop re-asserting target as soon as the apply (or its
  // rollback) is observed, so we never re-trigger the bad version afterwards.
  await lode.requestTarget(
    "0.0.2",
    (s) =>
      s.status === "updating" ||
      s.status === "rolling-back" ||
      (s.history ?? []).some((hh) => hh.version === "0.0.2" && hh.result === "bad"),
    { timeout: 15000, label: "begin applying unspawnable v0.0.2" },
  );

  // The spawn failure is a single strike: roll back to v0.0.1 and re-commit it.
  const rolled = await lode.waitForState(
    (s) =>
      s.status === "running" &&
      s.current === "0.0.1" &&
      (s.history ?? []).some((hh) => hh.version === "0.0.2" && hh.result === "bad"),
    { timeout: 20000, label: "rolled back to v0.0.1" },
  );

  expect(rolled.current).toBe("0.0.1");
  expect(rolled.last_good).toBe("0.0.1");
  expect((rolled.history ?? []).some((hh) => hh.version === "0.0.2" && hh.result === "bad")).toBe(true);
  // v0.0.2 never managed to start (spawn failed — no app banner)...
  expect(lode.countMatches(/\[app\] starting version=0\.0\.2/)).toBe(0);
  // ...and v0.0.1 was relaunched by the rollback (initial start + post-rollback).
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBeGreaterThanOrEqual(2);
  // The whole episode must not have taken down PID 1.
  expect(lode.exited).toBe(false);
});
