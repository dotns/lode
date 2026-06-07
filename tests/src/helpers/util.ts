// Shared utilities for the lode e2e suite: binary resolution, a clean child env,
// temp-dir management, sleeping, and a small spawn-and-collect helper for the
// short-lived `lode keygen` / `lode sign` invocations.

import { existsSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";

/** Absolute path to the lode binary under test (CI sets LODE_BIN=../target/debug/lode). */
export const LODE_BIN: string = (() => {
  const fromEnv = process.env.LODE_BIN;
  if (fromEnv && fromEnv.length > 0) {
    return isAbsolute(fromEnv) ? fromEnv : resolve(process.cwd(), fromEnv);
  }
  // Fallback: repo-root target/debug/lode, relative to this file (tests/src/helpers).
  return resolve(import.meta.dir, "../../../target/debug/lode");
})();

/**
 * Absolute path to the `lode-cli` multitool. lode is a multi-call binary: the
 * publisher/management subcommands (`keygen`/`sign`/…) are only available when it
 * is invoked under the `lode-cli` name. We materialise that name as a symlink to
 * `LODE_BIN` so the suite can sign artifacts (loader `lode` has no subcommands).
 */
export const LODE_CLI_BIN: string = (() => {
  const cliPath = join(dirname(LODE_BIN), "lode-cli");
  if (!existsSync(cliPath)) {
    try {
      symlinkSync(basename(LODE_BIN), cliPath); // relative: lode-cli -> lode
    } catch {
      /* created concurrently by another test file — fine */
    }
  }
  return cliPath;
})();

/** Sleep for `ms` milliseconds. */
export const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

/**
 * The environment handed to spawned lode processes: the host env with every
 * `LODE_*` variable stripped (so the harness drives lode purely via CLI flags and
 * a stray `LODE_DATA_DIR`/`LODE_MANIFEST`/etc. from the outer shell can never leak
 * in). `LODE_BIN` is not a lode config var, so dropping it is harmless.
 */
export function baseEnv(): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (v === undefined) continue;
    if (k.startsWith("LODE_")) continue;
    env[k] = v;
  }
  return env;
}

const createdTmpDirs: string[] = [];

/** Create a uniquely-named temp directory (tracked for best-effort cleanup). */
export function mkTmp(prefix: string): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  createdTmpDirs.push(dir);
  return dir;
}

/** Remove a temp directory (best-effort). */
export function rmTmp(dir: string): void {
  rmSync(dir, { recursive: true, force: true });
}

export interface RunResult {
  exitCode: number | null;
  stdout: string;
  stderr: string;
}

/** Spawn a short-lived command, fully collecting stdout/stderr and its exit code. */
export async function run(cmd: string[], opts: { cwd?: string; env?: Record<string, string> } = {}): Promise<RunResult> {
  const proc = Bun.spawn({
    cmd,
    cwd: opts.cwd,
    env: opts.env ?? baseEnv(),
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  return { exitCode, stdout, stderr };
}

/** Flip the last hex digit of a sha256 string so it no longer matches the file. */
export function flipHex(sha: string): string {
  const padded = sha.length >= 64 ? sha : sha.padEnd(64, "0");
  const last = padded.at(-1) ?? "0";
  const flipped = last === "0" ? "1" : "0";
  return padded.slice(0, -1) + flipped;
}
