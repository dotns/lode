// Scenario 29 (design §5 / source-adapters §5) — the GitHub Releases source. With
// `[update].github = owner/repo`, lode selects a release through the GitHub API
// (`/releases/latest` for `stable`, the newest prerelease for any other channel),
// maps the release's own asset list onto the internal manifest — version from the
// tag (minus a leading `v`), `digest` as sha256, `label` as the §1 signature — and
// then verifies + installs exactly like the native source. Driven against a local
// stand-in for the API (helpers/githubServer.ts), so no real network is touched.

import { mkdirSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { buildApp } from "./helpers/app.ts";
import { GithubServer } from "./helpers/githubServer.ts";
import { Harness } from "./helpers/harness.ts";
import { mkTmp, rmTmp, sleep } from "./helpers/util.ts";

let h: Harness;
let gh: GithubServer;
let buildDir: string;

afterEach(async () => {
  await h?.dispose();
  gh?.stop();
  if (buildDir) rmTmp(buildDir);
});

/** Build + sign a version and attach it to release `v<version>`. */
async function release(version: string, opts: { prerelease?: boolean } = {}): Promise<void> {
  const dir = join(buildDir, version);
  mkdirSync(dir, { recursive: true });
  const artifact = join(dir, "app.sh");
  buildApp(artifact, { version, mode: "service" });
  const signed = await h.signer.sign(artifact, version);
  gh.publish(`v${version}`, {
    artifactPath: artifact,
    name: "app.sh",
    sha256: signed.sha256,
    sig: signed.sig,
    prerelease: opts.prerelease,
  });
}

/** lode wired to the GitHub stand-in instead of the harness's native manifest. */
function githubArgs(channel: string): string[] {
  return [
    "--app",
    "e2e-app",
    "--dir",
    h.dataDir,
    "--github",
    "acme/e2e-app",
    "--github-api",
    gh.apiUrl,
    "--channel",
    channel,
    "--asset",
    "app.sh",
    "--run",
    "./app.sh",
    "--exec",
    "./app.sh",
    "--log-level",
    "info",
    ...h.trustArgs("enforce"),
    "--policy",
    "auto",
    "--interval",
    "1",
    "--readiness",
    "none",
    "--health-grace",
    "1",
    "--stop-timeout",
    "5",
  ];
}

test("stable follows /releases/latest, auto-updates on a new release, and ignores prereleases", async () => {
  h = await Harness.start();
  gh = GithubServer.start();
  buildDir = mkTmp("lode-gh-build-");
  await release("0.0.1");

  const lode = h.runLodeRaw(githubArgs("stable"));
  // Version = tag minus its leading `v`; the label signature verified under enforce.
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1 from the GitHub source",
  });
  await lode.waitForStdout(/\[app\] starting version=0\.0\.1/, { label: "v0.0.1 up" });

  // A newer full release moves `latest`; policy=auto applies it unaided.
  await release("0.0.2");
  const swapped = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 20000,
    label: "auto-applied v0.0.2",
  });
  expect(swapped.last_good).toBe("0.0.2");

  // A prerelease never moves `stable` (GitHub's `latest` skips prereleases).
  await release("0.0.3-beta.1", { prerelease: true });
  await sleep(3000);
  expect(lode.readState()?.current).toBe("0.0.2");
  expect(lode.countMatches(/\[app\] starting version=0\.0\.3/)).toBe(0);
  expect(lode.exited).toBe(false);
});

test("a non-stable channel follows the newest prerelease", async () => {
  h = await Harness.start();
  gh = GithubServer.start();
  buildDir = mkTmp("lode-gh-build-");
  await release("0.0.1");
  await release("0.0.2-beta.1", { prerelease: true });

  const lode = h.runLodeRaw(githubArgs("beta"));
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2-beta.1", {
    timeout: 20000,
    label: "running the newest prerelease",
  });
  expect(lode.readState()?.channel).toBe("beta");
  expect(lode.exited).toBe(false);
});
