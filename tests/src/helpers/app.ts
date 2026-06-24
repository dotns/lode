// Versioned app-artifact builder. Reads the POSIX-sh template at
// tests/src/fixtures/app.sh and bakes the BUILD_* lines to produce a distinct
// `raw` artifact per (version, mode). The result is the file lode downloads and runs.

import { chmodSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const TEMPLATE = join(import.meta.dir, "..", "fixtures", "app.sh");

export type AppMode = "service" | "exit" | "update-on-exit";

export interface BuildOpts {
  version: string;
  /** service = long-running; exit = print then exit(code); update-on-exit = write state.target then exit(0). */
  mode?: AppMode;
  /** Exit code for mode=exit (0 = clean stop, non-zero = crash). */
  exitCode?: number;
  /** Version to request for mode=update-on-exit. */
  target?: string;
  /** service + readiness=state: defer the serving ready (-0) until $LODE_DIR/ready_ok exists. */
  gate?: boolean;
  /** service + readiness=state: defer the prepare ack (-2) until $LODE_DIR/prepare_ok exists. */
  preGate?: boolean;
}

/** Build a versioned app artifact at `dest` (chmod +x). */
export function buildApp(dest: string, opts: BuildOpts): void {
  const { version, mode = "service", exitCode = 0, target = "", gate = false, preGate = false } = opts;
  let s = readFileSync(TEMPLATE, "utf8");
  s = s
    .replace(/^BUILD_VERSION=.*$/m, `BUILD_VERSION="${version}"`)
    .replace(/^BUILD_MODE=.*$/m, `BUILD_MODE="${mode}"`)
    .replace(/^BUILD_EXIT_CODE=.*$/m, `BUILD_EXIT_CODE="${exitCode}"`)
    .replace(/^BUILD_TARGET=.*$/m, `BUILD_TARGET="${target}"`)
    .replace(/^BUILD_GATE=.*$/m, `BUILD_GATE="${gate ? "1" : "0"}"`)
    .replace(/^BUILD_PREPARE_GATE=.*$/m, `BUILD_PREPARE_GATE="${preGate ? "1" : "0"}"`);
  writeFileSync(dest, s);
  chmodSync(dest, 0o755);
}
