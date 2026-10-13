#!/usr/bin/env node
// Runs `vitest run` with the arguments given to `pnpm test`. pnpm forwards a literal `--` (once per
// nested `pnpm test -- ...`), and vitest treats everything after `--` as ignored extra arguments,
// so `pnpm test -- --testTimeout=30000` used to run with the default timeout. Drop those markers.
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

const args = process.argv.slice(2).filter((a) => a !== "--");
const pkg = createRequire(import.meta.url).resolve("vitest/package.json");
const vitest = join(dirname(pkg), "vitest.mjs");
const r = spawnSync(process.execPath, [vitest, "run", ...args], { stdio: "inherit" });
process.exit(r.status ?? 1);
