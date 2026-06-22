// LodeRunner — drives one real `lode` process: Bun.spawn the binary, stream and
// timestamp its stdout/stderr, read state.json, write app-style requests
// (state.target), send signals, and capture the EXIT CODE. All waits are bounded
// and, on timeout, dump the captured output + last state for a debuggable failure.

import { readFileSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { LODE_BIN, sleep } from "./util.ts";

export interface HistoryEntry {
  version: string;
  at: string;
  result: "good" | "bad";
}

export interface LodeState {
  current?: string;
  last_good?: string;
  available?: string;
  channel?: string;
  status?: "starting" | "running" | "updating" | "rolling-back" | "stopping" | "stopped" | "error";
  pid?: number;
  last_check?: string;
  last_error?: string;
  history?: HistoryEntry[];
  config_generation?: number;
  target?: string;
  restart_nonce?: number;
  ready?: string;
}

export interface ExitInfo {
  code: number | null;
  signal: string | null;
}

interface LineEvent {
  t: number;
  line: string;
}

export interface WaitOpts {
  timeout?: number;
  interval?: number;
  label?: string;
}

export class LodeRunner {
  readonly dataDir: string;
  readonly #proc: Bun.Subprocess;
  #stdout = "";
  #stderr = "";
  #stdoutTail = "";
  readonly stdoutLines: LineEvent[] = [];

  constructor(args: string[], dataDir: string, env: Record<string, string>) {
    this.dataDir = dataDir;
    this.#proc = Bun.spawn({
      cmd: [LODE_BIN, ...args],
      env,
      stdout: "pipe",
      stderr: "pipe",
    });
    void this.#pump(this.#proc.stdout as ReadableStream<Uint8Array>, true);
    void this.#pump(this.#proc.stderr as ReadableStream<Uint8Array>, false);
  }

  async #pump(stream: ReadableStream<Uint8Array>, isStdout: boolean): Promise<void> {
    const dec = new TextDecoder();
    try {
      for await (const chunk of stream) {
        const text = dec.decode(chunk, { stream: true });
        if (isStdout) {
          this.#stdout += text;
          this.#ingest(text);
        } else {
          this.#stderr += text;
        }
      }
    } catch {
      // stream closed/aborted on process exit — nothing to do.
    }
  }

  #ingest(text: string): void {
    this.#stdoutTail += text;
    let idx: number;
    while ((idx = this.#stdoutTail.indexOf("\n")) >= 0) {
      const line = this.#stdoutTail.slice(0, idx);
      this.#stdoutTail = this.#stdoutTail.slice(idx + 1);
      this.stdoutLines.push({ t: Date.now(), line });
    }
  }

  get stdout(): string {
    return this.#stdout;
  }

  get stderr(): string {
    return this.#stderr;
  }

  get pid(): number {
    return this.#proc.pid;
  }

  /** Has the lode process exited (cleanly or via signal)? */
  get exited(): boolean {
    return this.#proc.exitCode !== null || this.#proc.signalCode !== null;
  }

  /** Timestamps (ms) of every stdout line matching `re` — e.g. app "starting" lines. */
  matchTimes(re: RegExp): number[] {
    return this.stdoutLines.filter((e) => re.test(e.line)).map((e) => e.t);
  }

  /** Count stdout lines matching `re`. */
  countMatches(re: RegExp): number {
    return this.stdoutLines.filter((e) => re.test(e.line)).length;
  }

  /** Read state.json (null if absent/unparsable mid-write). */
  readState(): LodeState | null {
    try {
      return JSON.parse(readFileSync(join(this.dataDir, "state.json"), "utf8")) as LodeState;
    } catch {
      return null;
    }
  }

  /** Atomically set a field in state.json (the way an app would request work). */
  writeStateField(key: keyof LodeState, value: string | number): void {
    const path = join(this.dataDir, "state.json");
    let st: Record<string, unknown> = {};
    try {
      st = JSON.parse(readFileSync(path, "utf8")) as Record<string, unknown>;
    } catch {
      st = {};
    }
    st[key] = value;
    const tmp = `${path}.test.${process.pid}.tmp`;
    writeFileSync(tmp, JSON.stringify(st, null, 2));
    renameSync(tmp, path);
  }

  #fail(msg: string): never {
    throw new Error(
      `${msg}\nlast state: ${JSON.stringify(this.readState())}\nexited: ${this.exited} (code=${this.#proc.exitCode} signal=${this.#proc.signalCode})\n--- stdout ---\n${this.#stdout}\n--- stderr ---\n${this.#stderr}`,
    );
  }

  /** Poll state.json until `pred` holds; returns the matching state or throws on timeout. */
  async waitForState(pred: (s: LodeState) => boolean, opts: WaitOpts = {}): Promise<LodeState> {
    const { timeout = 15000, interval = 100, label = "" } = opts;
    const start = Date.now();
    while (Date.now() - start < timeout) {
      const st = this.readState();
      if (st && pred(st)) return st;
      await sleep(interval);
    }
    this.#fail(`waitForState timed out (${timeout}ms) ${label}`);
  }

  /** Wait until accumulated stdout matches `re`, or throw on timeout. */
  async waitForStdout(re: RegExp, opts: WaitOpts = {}): Promise<void> {
    const { timeout = 15000, interval = 100, label = "" } = opts;
    const start = Date.now();
    while (Date.now() - start < timeout) {
      if (re.test(this.#stdout)) return;
      await sleep(interval);
    }
    this.#fail(`waitForStdout timed out (${timeout}ms) ${label} re=${re}`);
  }

  /**
   * Repeatedly (re)assert `state.target = version` until `until(state)` holds —
   * robust against lode's own concurrent state writes (policy checks). Stops
   * re-asserting the moment the apply is observed, so it never re-triggers a
   * second update after a rollback.
   */
  async requestTarget(version: string, until: (s: LodeState) => boolean, opts: WaitOpts = {}): Promise<LodeState> {
    const { timeout = 15000, interval = 250, label = "" } = opts;
    const start = Date.now();
    while (Date.now() - start < timeout) {
      const st = this.readState() ?? {};
      if (until(st)) return st;
      if (st.target !== version) this.writeStateField("target", version);
      await sleep(interval);
    }
    this.#fail(`requestTarget(${version}) not observed (${timeout}ms) ${label}`);
  }

  /** Send a signal to the lode process. */
  signal(sig: NodeJS.Signals): void {
    try {
      this.#proc.kill(sig);
    } catch {
      // already exited
    }
  }

  /** Wait for lode to exit; returns its code/signal, or throws on timeout. */
  async waitExit(timeout = 15000): Promise<ExitInfo> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timed = new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error("__lode_wait_exit_timeout__")), timeout);
    });
    try {
      await Promise.race([this.#proc.exited, timed]);
    } catch (e) {
      if (e instanceof Error && e.message === "__lode_wait_exit_timeout__") {
        this.#fail(`lode did not exit within ${timeout}ms`);
      }
      throw e;
    } finally {
      if (timer) clearTimeout(timer);
    }
    return { code: this.#proc.exitCode, signal: this.#proc.signalCode };
  }

  /** Best-effort teardown: SIGTERM (so lode stops its child), then SIGKILL. */
  async dispose(): Promise<void> {
    if (this.exited) return;
    this.signal("SIGTERM");
    await Promise.race([this.#proc.exited, sleep(3000)]);
    if (!this.exited) {
      this.signal("SIGKILL");
      await Promise.race([this.#proc.exited, sleep(1000)]);
    }
  }
}
