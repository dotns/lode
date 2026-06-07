// Scenario 2 — CLI passthrough: `lode <args>` exec-replaces into [command].exec +
// args. We assert both the app's stdout AND its exit code propagate through the
// exec. (`lode print hello 3` => app prints "hello", exits 3.)

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("lode <args> execs the app, propagating stdout and exit code", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  // No installed version yet => exec passthrough bootstraps (fetch+verify+install),
  // then exec-replaces into `app.sh print hello 3`.
  const lode = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "print", "hello", "3"]);

  const exit = await lode.waitExit(20000);
  expect(exit.code).toBe(3);
  expect(lode.stdout).toContain("hello");
});
