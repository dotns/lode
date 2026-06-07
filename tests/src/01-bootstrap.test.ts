// Scenario 1 — bootstrap: empty data dir => lode fetches the manifest, selects the
// named asset (`[update].asset`), verifies sha256 + ed25519, installs, and runs it. We assert
// the app ACTUALLY runs (state.current==v0.0.1, status running, pid live, and the
// app's own startup line on stdout).

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("bootstrap installs + verifies + runs the channel-latest version", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  // require_signature=enforce exercises the ed25519 path (not just sha256).
  const lode = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "--readiness", "none"]);

  const st = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "bootstrap -> running v0.0.1",
  });

  expect(st.current).toBe("0.0.1");
  expect(st.last_good).toBe("0.0.1");
  expect(st.status).toBe("running");
  expect(typeof st.pid).toBe("number");

  // The app really launched (verified artifact actually executes).
  await lode.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "app started" });

  // The recorded pid is a live process.
  expect(() => process.kill(st.pid as number, 0)).not.toThrow();
  expect(lode.exited).toBe(false);
});
