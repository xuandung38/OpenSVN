import { spawnSync } from "node:child_process";
import { expect, test } from "vitest";

// Runs the forbidden-sink scan as part of `pnpm test` so CI enforces it without a workflow change.
test("source has no innerHTML/eval/dangerouslySetInnerHTML sinks", () => {
  const r = spawnSync(process.execPath, ["scripts/lint-safety.mjs"], { encoding: "utf8" });
  expect(r.stderr + r.stdout).toContain("lint:safety ok");
  expect(r.status).toBe(0);
});
