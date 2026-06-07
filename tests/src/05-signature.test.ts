// Scenario 5 — signature/integrity rejection under require_signature=enforce: an
// update to a tampered (sha256 mismatch) or unsigned artifact must be REFUSED, and
// the currently-running version left untouched (state.current unchanged, a
// last_error surfaced, the app still serving the old version).

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

async function bootstrapV1(): Promise<ReturnType<Harness["runLode"]>> {
  await h.publish("0.0.1", { mode: "service", latest: true });
  // policy=off so lode stays idle after bootstrap — the only state writer besides
  // our single target request, which makes "stayed on v0.0.1" unambiguous.
  const lode = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "--readiness", "none", "--stop-timeout", "5"]);
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1",
  });
  await lode.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "v0.0.1 running" });
  return lode;
}

test("rejects a sha256-mismatched update artifact (enforce); stays on v0.0.1", async () => {
  h = await Harness.start();
  const lode = await bootstrapV1();

  // Tampered: served bytes do not match the manifest's declared sha256.
  await h.publish("0.0.2", { mode: "service", latest: true, tamperSha: true });

  const st = await lode.requestTarget("0.0.2", (s) => !!s.last_error, {
    timeout: 15000,
    label: "install refused",
  });

  expect(st.current).toBe("0.0.1");
  expect(st.last_error ?? "").toMatch(/sha256|mismatch/i);
  // v0.0.2 never started; the old version is still the one running.
  expect(lode.countMatches(/\[app\] starting version=0\.0\.2/)).toBe(0);
  expect(lode.readState()?.current).toBe("0.0.1");
  expect(lode.exited).toBe(false);
});

test("installs under enforce when the catalog is unsigned but the artifact is signed", async () => {
  // The catalog signature is verify-if-present, never required: a manifest with no
  // top-level `sig` (e.g. a GitHub release, or a publisher who signs only artifacts)
  // must still bootstrap+run under enforce, because the per-artifact signature — which
  // IS policy-gated — binds the download, and the downgrade floor guards `latest`.
  h = await Harness.start({ signCatalog: false });
  await h.publish("0.0.1", { mode: "service", latest: true });

  const lode = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "--readiness", "none"]);
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1 (unsigned catalog, signed artifact, enforce)",
  });
  await lode.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "v0.0.1 running" });
  expect(lode.readState()?.current).toBe("0.0.1");
  expect(lode.exited).toBe(false);
});

test("rejects an unsigned update artifact under enforce; stays on v0.0.1", async () => {
  h = await Harness.start();
  const lode = await bootstrapV1();

  // Correct sha256 but no signature — must be refused under enforce.
  await h.publish("0.0.2", { mode: "service", latest: true, omitSig: true });

  const st = await lode.requestTarget("0.0.2", (s) => !!s.last_error, {
    timeout: 15000,
    label: "install refused (unsigned)",
  });

  expect(st.current).toBe("0.0.1");
  expect(st.last_error ?? "").toMatch(/signature|enforce/i);
  expect(lode.countMatches(/\[app\] starting version=0\.0\.2/)).toBe(0);
  expect(lode.exited).toBe(false);
});
