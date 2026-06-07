// Publisher signing wired to the real `lode-cli` binary: `lode-cli keygen`
// generates an ed25519 keypair, `lode-cli sign` produces the sha256 + ed25519
// signature over the §1 canonical message (v3 — binds the asset filename `name`
// = the artifact basename, the version AND the sha256; NOT platform/format/url/
// entry). (lode is a multi-call binary; signing lives under the `lode-cli`
// name — see LODE_CLI_BIN.) The trusted-key string (`key_id:base64`) is fed back
// into lode via --trusted-keys so install-time verification can succeed.

import { readFileSync } from "node:fs";
import { join } from "node:path";

import { LODE_CLI_BIN, run } from "./util.ts";

export interface Signature {
  sha256: string;
  sig: string;
}

export class Signer {
  readonly keyId: string;
  readonly pub: string;
  readonly privPath: string;

  private constructor(keyId: string, pub: string, privPath: string) {
    this.keyId = keyId;
    this.pub = pub;
    this.privPath = privPath;
  }

  /** Generate a keypair under `keysDir` via `lode keygen --out`. */
  static async create(keysDir: string): Promise<Signer> {
    const prefix = join(keysDir, "publisher");
    const r = await run([LODE_CLI_BIN, "keygen", "--out", prefix]);
    if (r.exitCode !== 0) throw new Error(`lode keygen failed (${r.exitCode}): ${r.stderr}`);
    // `<prefix>.pub` is written as "<key_id> <base64>\n"; `<prefix>.key` is the raw base64 seed.
    const pubLine = readFileSync(`${prefix}.pub`, "utf8").trim();
    const [keyId, pub] = pubLine.split(/\s+/);
    if (!keyId || !pub) throw new Error(`unexpected keygen .pub format: ${pubLine}`);
    return new Signer(keyId, pub, `${prefix}.key`);
  }

  /** The trusted-key entry for lode's --trusted-keys / [trust].trusted_keys. */
  get trustedKey(): string {
    return `${this.keyId}:${this.pub}`;
  }

  /** Sign one artifact, returning its sha256 + base64 signature. The signature message
   * binds the asset filename (= `basename(artifactPath)`) + version + sha256; lode
   * reconstructs it from the manifest asset `name`, so that name must equal this
   * artifact's basename (and the filename the test serves / selects via
   * `[update].asset`). */
  async sign(artifactPath: string, version: string): Promise<Signature> {
    const r = await run([LODE_CLI_BIN, "sign", artifactPath, "--version", version, "--key", this.privPath]);
    if (r.exitCode !== 0) throw new Error(`lode sign failed (${r.exitCode}): ${r.stderr}\n${r.stdout}`);
    const sha = r.stdout.match(/sha256:\s*([0-9a-fA-F]{64})/)?.[1];
    const sig = r.stdout.match(/sig:\s*([A-Za-z0-9+/=]+)/)?.[1];
    if (!sha || !sig) throw new Error(`could not parse sha256/sig from lode sign output:\n${r.stdout}`);
    return { sha256: sha.toLowerCase(), sig };
  }

  /** Sign a complete manifest.json in place (stamp its top-level `key_id` + `sig`)
   * via `lode-cli manifest-sign`. Required whenever the loader verifies the
   * manifest-level signature — i.e. require_signature=enforce, or auto with a
   * trusted key configured. Re-run after every republish, since the signature
   * covers the channel/version catalog (which changes each publish). */
  async signManifest(manifestPath: string): Promise<void> {
    const r = await run([LODE_CLI_BIN, "manifest-sign", "--into", manifestPath, "--key", this.privPath]);
    if (r.exitCode !== 0) throw new Error(`lode manifest-sign failed (${r.exitCode}): ${r.stderr}\n${r.stdout}`);
  }
}
