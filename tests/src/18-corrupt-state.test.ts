// Scenario 18 — corrupt state.json tolerance (P0): lode runs as PID 1, so a
// corrupt/torn state.json must never kill the supervisor — neither at boot (the
// corrupt file survives restarts on the volume, so an exit is a permanent
// crash-loop) nor mid-run (the ~1s state poll). lode warns, quarantines the file
// to state.json.corrupt (evidence preserved, next write starts clean), and keeps
// supervising.

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

/** A torn write: JSON cut off right after the key — invalid / unparsable. */
const GARBAGE = '{"current":';

test("boot: a pre-existing corrupt state.json is quarantined and the app still comes up", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  // Pre-seed the data dir with garbage — pre-fix, the boot read ?-propagates the
  // serde error and PID 1 exits before the app ever starts.
  writeFileSync(join(h.dataDir, "state.json"), GARBAGE);

  const lode = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "--readiness", "none"]);

  const st = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "boot through corrupt state.json -> running v0.0.1",
  });
  expect(st.current).toBe("0.0.1");
  expect(lode.exited).toBe(false);

  // The app really launched despite the corrupt file.
  await lode.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "app started" });

  // Evidence preserved: the corrupt bytes were moved aside, byte-for-byte.
  const quarantined = join(h.dataDir, "state.json.corrupt");
  expect(existsSync(quarantined)).toBe(true);
  expect(readFileSync(quarantined, "utf8")).toBe(GARBAGE);
});

test("mid-run: corruption is quarantined and the supervise loop keeps servicing requests", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  const lode = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "--readiness", "none"]);
  const st = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "clean boot -> running v0.0.1",
  });
  const appPid = st.pid as number;

  // Corrupt state.json under the running supervisor (a torn app write). The ~1s
  // state poll reads it — pre-fix, the serde error exits PID 1.
  writeFileSync(join(h.dataDir, "state.json"), GARBAGE);

  // The poll quarantines the file — proof the corrupt read happened and was survived.
  const quarantined = join(h.dataDir, "state.json.corrupt");
  const start = Date.now();
  while (!existsSync(quarantined) && Date.now() - start < 15000) await sleep(100);
  expect(existsSync(quarantined)).toBe(true);
  expect(lode.exited).toBe(false);

  // The app was never disturbed by the corruption…
  expect(() => process.kill(appPid, 0)).not.toThrow();

  // …and the loop still services app requests: a restart_nonce bump (written to
  // the now-clean state.json) gracefully cycles the child onto a new pid.
  lode.writeStateField("restart_nonce", 1);
  const cycled = await lode.waitForState(
    (s) => s.status === "running" && typeof s.pid === "number" && s.pid !== appPid,
    { timeout: 20000, label: "restart serviced after corruption" },
  );
  expect(cycled.current).toBe("0.0.1");
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBeGreaterThanOrEqual(2);
  expect(lode.exited).toBe(false);
});
