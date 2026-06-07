// A local lode/v1 manifest + artifact server (Bun.serve) over a WRITABLE temp dir.
// Everything is served fresh from disk, so publish() taking effect at runtime is
// just a file write — no caching, no restart. Binds 127.0.0.1:0 (ephemeral port),
// so the suite never touches the real network.

import { copyFileSync, existsSync, mkdirSync, renameSync, writeFileSync } from "node:fs";
import { join, normalize, sep } from "node:path";

import { LODE_CLI_BIN, mkTmp, rmTmp } from "./util.ts";

// One per-version asset, keyed by its filename (`name`) — the source-agnostic
// selection key (§3) and the identity the §1 signature binds.
interface Asset {
  name: string;
  url: string;
  sha256: string;
  sig?: string;
  key_id?: string;
  entry?: string;
  size?: number;
}

interface Manifest {
  schema: string;
  name: string;
  channels: Record<string, { latest: string }>;
  versions: Record<string, { min_lode: string; notes: string; assets: Asset[] }>;
}

export interface PublishOpts {
  artifactPath: string;
  /** The asset filename to serve — the selection key matched against
   *  `[update].asset`, and the basename the signature binds. Defaults to "app.sh". */
  name?: string;
  sha256: string;
  sig?: string;
  keyId?: string;
  /** Advisory in-archive entry (§4); defaults to `name`. */
  entry?: string;
  /** Point channels.stable.latest at this version (default true). */
  latest?: boolean;
}

export class ManifestServer {
  readonly name: string;
  #www: string;
  #server: ReturnType<typeof Bun.serve>;
  #manifest: Manifest;
  /** When set, every written manifest.json is signed in place via `lode-cli manifest-sign`. */
  #signKeyPath?: string;

  private constructor(name: string, www: string, server: ReturnType<typeof Bun.serve>, manifest: Manifest, signKeyPath?: string) {
    this.name = name;
    this.#www = www;
    this.#server = server;
    this.#manifest = manifest;
    this.#signKeyPath = signKeyPath;
  }

  /**
   * Start the server. When `signKeyPath` is given, each manifest.json is stamped
   * with a top-level `key_id` + `sig` (over the canonical message) before being
   * served, so lode's manifest-signature verification passes under
   * `require_signature = auto|enforce`.
   */
  static start(name: string, signKeyPath?: string): ManifestServer {
    const www = mkTmp("lode-www-");
    const manifest: Manifest = { schema: "lode/v1", name, channels: {}, versions: {} };
    const wwwRoot = normalize(www);
    const server = Bun.serve({
      port: 0,
      hostname: "127.0.0.1",
      async fetch(req) {
        const url = new URL(req.url);
        const path = normalize(join(wwwRoot, decodeURIComponent(url.pathname)));
        // Contain serving to the www root (defeat ../ traversal).
        if (path !== wwwRoot && !path.startsWith(wwwRoot + sep)) {
          return new Response("forbidden", { status: 403 });
        }
        const file = Bun.file(path);
        if (!(await file.exists())) return new Response("not found", { status: 404 });
        return new Response(file);
      },
    });
    return new ManifestServer(name, www, server, manifest, signKeyPath);
  }

  get url(): string {
    return `http://127.0.0.1:${this.#server.port}`;
  }

  get manifestUrl(): string {
    return `${this.url}/manifest.json`;
  }

  /** Add/replace a version: copy its artifact into the tree (served under its asset
   *  `name`, the selection key) and update manifest.json. */
  publish(version: string, opts: PublishOpts): void {
    const name = opts.name ?? "app.sh";
    const entry = opts.entry ?? name;
    const dir = join(this.#www, "artifacts", version);
    mkdirSync(dir, { recursive: true });
    // Serve the asset under its `name` so the published filename == the selection
    // key (`[update].asset`) == the basename the signature was computed over.
    copyFileSync(opts.artifactPath, join(dir, name));

    const asset: Asset = {
      name,
      url: `${this.url}/artifacts/${version}/${name}`,
      sha256: opts.sha256,
      entry,
    };
    if (opts.sig) asset.sig = opts.sig;
    if (opts.keyId) asset.key_id = opts.keyId;

    this.#manifest.versions[version] = {
      // min_lode must be satisfied by the loader under test (0.0.1).
      min_lode: "0.0.1",
      notes: `e2e ${version}`,
      assets: [asset],
    };
    if (opts.latest ?? true) {
      this.#manifest.channels.stable = { latest: version };
    }
    this.#writeManifest();
  }

  #writeManifest(): void {
    if (!existsSync(this.#www)) mkdirSync(this.#www, { recursive: true });
    // Atomic, synchronous write so a concurrent lode fetch never sees a partial
    // manifest and the new content is visible the instant publish() returns. When
    // signing is enabled, the top-level key_id+sig are stamped onto the temp file
    // BEFORE the rename, so the served manifest is always atomically signed (no
    // unsigned-then-signed window for a polling lode to catch).
    const dest = join(this.#www, "manifest.json");
    const tmp = `${dest}.tmp`;
    writeFileSync(tmp, JSON.stringify(this.#manifest, null, 2));
    if (this.#signKeyPath) {
      const r = Bun.spawnSync({ cmd: [LODE_CLI_BIN, "manifest-sign", "--into", tmp, "--key", this.#signKeyPath] });
      if (r.exitCode !== 0) {
        throw new Error(`manifest-sign failed (${r.exitCode}): ${new TextDecoder().decode(r.stderr)}`);
      }
    }
    renameSync(tmp, dest);
  }

  stop(): void {
    this.#server.stop(true);
    rmTmp(this.#www);
  }
}
