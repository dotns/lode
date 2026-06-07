// Harness — one self-contained world per test: a writable manifest/artifact
// server, a publisher keypair, a temp data dir, and a factory for lode processes.
// publish() builds+signs+serves a version in one call; runLode() spawns lode wired
// to this world. dispose() tears everything down (graceful child stop included).

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { type AppMode, buildApp } from "./app.ts";
import { LodeRunner } from "./lode.ts";
import { ManifestServer } from "./manifestServer.ts";
import { Signer } from "./sign.ts";
import { baseEnv, flipHex, mkTmp, rmTmp } from "./util.ts";

export interface PublishVersionOpts {
  mode?: AppMode;
  exitCode?: number;
  target?: string;
  gate?: boolean;
  /** Make channels.stable.latest point at this version (default true). */
  latest?: boolean;
  /** Serve a sha256 that does NOT match the bytes (tampered artifact). */
  tamperSha?: boolean;
  /** Publish without a signature (to be rejected under require_signature=enforce). */
  omitSig?: boolean;
}

const APP_NAME = "e2e-app";
/** The asset filename every version is served under — the selection key (§3) that
 *  lode matches via `[update].asset`, and the basename the §1 signature binds. */
const ASSET_NAME = "app.sh";

export class Harness {
  readonly server: ManifestServer;
  readonly signer: Signer;
  readonly dataDir: string;
  readonly #buildDir: string;
  readonly #tmps: string[] = [];
  readonly #lodes: LodeRunner[] = [];

  private constructor(server: ManifestServer, signer: Signer, dataDir: string, buildDir: string, keysDir: string) {
    this.server = server;
    this.signer = signer;
    this.dataDir = dataDir;
    this.#buildDir = buildDir;
    this.#tmps.push(dataDir, buildDir, keysDir);
  }

  static async start(): Promise<Harness> {
    const dataDir = mkTmp("lode-data-");
    const buildDir = mkTmp("lode-build-");
    const keysDir = mkTmp("lode-keys-");
    const signer = await Signer.create(keysDir);
    // The server signs every manifest.json with the publisher key, so the new
    // manifest-level signature verification passes under require_signature.
    const server = ManifestServer.start(APP_NAME, signer.privPath);
    return new Harness(server, signer, dataDir, buildDir, keysDir);
  }

  get trustedKey(): string {
    return this.signer.trustedKey;
  }

  /** Build, sign, and serve a version. With tamperSha/omitSig, produce a bad artifact. */
  async publish(version: string, opts: PublishVersionOpts = {}): Promise<void> {
    // Build under a per-version dir so the artifact's basename is exactly the asset
    // filename (ASSET_NAME): the §1 signature binds that basename, and lode
    // reconstructs the signed message from the manifest asset `name`.
    const versionDir = join(this.#buildDir, version);
    mkdirSync(versionDir, { recursive: true });
    const artifactPath = join(versionDir, ASSET_NAME);
    buildApp(artifactPath, {
      version,
      mode: opts.mode ?? "service",
      exitCode: opts.exitCode ?? 0,
      target: opts.target ?? "",
      gate: opts.gate ?? false,
    });

    // The signature binds the asset filename (basename) + version + sha256; it
    // never sees url/entry, so they are free to vary. `name` is the selection key.
    const signed = await this.signer.sign(artifactPath, version);
    const sha256 = opts.tamperSha ? flipHex(signed.sha256) : signed.sha256;
    const sig = opts.omitSig ? undefined : signed.sig;
    const keyId = opts.omitSig ? undefined : this.signer.keyId;

    this.server.publish(version, {
      artifactPath,
      name: ASSET_NAME,
      sha256,
      sig,
      keyId,
      entry: ASSET_NAME,
      latest: opts.latest ?? true,
    });
  }

  /** Base CLI flags shared by every lode invocation in this world. */
  baseArgs(): string[] {
    return [
      "--app",
      this.server.name,
      "--data-dir",
      this.dataDir,
      "--manifest",
      this.server.manifestUrl,
      "--asset",
      ASSET_NAME,
      "--run",
      "{entry}",
      "--exec",
      "{entry}",
      "--log-level",
      "info",
    ];
  }

  /** --require-signature enforce + this world's trusted key. */
  trustArgs(mode: "off" | "auto" | "enforce" = "enforce"): string[] {
    return ["--require-signature", mode, "--trusted-keys", this.trustedKey];
  }

  /** Spawn a lode process wired to this world (baseArgs + extra flags). */
  runLode(extraArgs: string[]): LodeRunner {
    const runner = new LodeRunner([...this.baseArgs(), ...extraArgs], this.dataDir, baseEnv());
    this.#lodes.push(runner);
    return runner;
  }

  /** Drop the readiness gate file so a gated app announces readiness. */
  openReadinessGate(): void {
    writeFileSync(join(this.dataDir, "ready_ok"), "ok");
  }

  async dispose(): Promise<void> {
    for (const l of this.#lodes) await l.dispose();
    this.server.stop();
    for (const t of this.#tmps) rmTmp(t);
  }
}
