// Soak of the real app (#21): a large working copy stays open with the file watcher and the
// background poller (incoming changes, every minute) running, while edits land on disk every
// 15 s, another user commits every minute and the app updates every 2 minutes. The memory of the
// app process tree is sampled every second. It must stay under the RSS limit and must not creep:
// the mean of the last fifth of the run may exceed the mean of the second fifth (after warm-up) by
// at most OPENSVN_SOAK_MAX_GROWTH_MB.
//
// OPENSVN_SOAK_MINUTES (default 10; the weekly job runs 120), OPENSVN_SOAK_FILES (default 50000),
// OPENSVN_SOAK_LIMIT_MB (400), OPENSVN_SOAK_MAX_GROWTH_MB (32). OPENSVN_SOAK_APP_ENV adds
// variables to the app's environment, e.g. "MALLOC_ARENA_MAX=2" or
// "LD_PRELOAD=/usr/lib/x86_64-linux-gnu/libjemalloc.so.2", to compare allocators. Results go to
// e2e-artifacts/soak-report.{md,json}.

import assert from "node:assert/strict";
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { after, before, test } from "node:test";
import { APP, App, ScratchRepo, sleep } from "./harness.mjs";
import {
  appEnvFrom,
  checkout,
  commitUrls,
  invoke,
  MemorySampler,
  mb,
  saveFailure,
  seedLarge,
  startKeyring,
  svnStatus,
  writeArtifact,
} from "./reliability.mjs";

const MINUTES = Number(process.env.OPENSVN_SOAK_MINUTES ?? 10);
const FILES = Number(process.env.OPENSVN_SOAK_FILES ?? 50_000);
const LIMIT_MB = Number(process.env.OPENSVN_SOAK_LIMIT_MB ?? 400);
const MAX_GROWTH_MB = Number(process.env.OPENSVN_SOAK_MAX_GROWTH_MB ?? 32);
const APP_ENV = appEnvFrom("OPENSVN_SOAK_APP_ENV");
const PER_DIR = 100;
const fileName = (i) => `d${Math.floor(i / PER_DIR)}/f${i}.txt`;

/** @type {{ env: Record<string, string>, stop(): void }} */
let keyring;
before(() => {
  keyring = startKeyring();
});
after(() => keyring?.stop());

/** A commit by someone else: a few files near the end of the tree (never the local edits). */
function commitElsewhere(repo, n) {
  const files = {};
  for (let k = 0; k < 3; k++) {
    const i = FILES - 1 - ((n * 3 + k) % 500);
    files[`${repo.url}/trunk/${fileName(i)}`] = `file ${i}\nchanged elsewhere ${n}\n`;
  }
  commitUrls(repo, `Elsewhere ${n}`, files);
}

const mean = (xs) => xs.reduce((a, b) => a + b, 0) / Math.max(1, xs.length);

test(`soak: ${MINUTES} min on a ${FILES}-file working copy, no memory creep`, async () => {
  const repo = new ScratchRepo("soak");
  let app;
  let sampler;
  const counts = { edits: 0, commitsElsewhere: 0, updates: 0, failedUpdates: 0 };
  try {
    seedLarge(repo, FILES, fileName);
    app = await App.start("soak", { ...keyring.env, ...APP_ENV });
    const wc = await checkout(app, repo, "trunk", 1_800_000);
    const set = await invoke(app, "settings_set", { patch: { "general.checkIntervalMin": 1 } });
    assert.ok(set.ok, `settings_set: ${JSON.stringify(set.err)}`);

    sampler = new MemorySampler(app, 1000);
    sampler.mark("soak");
    const start = Date.now();
    const end = start + MINUTES * 60_000;
    let nextEdit = start;
    let nextElsewhere = start + 30_000;
    let nextUpdate = start + 120_000;
    // Local edits rotate over the first 2000 files, the other user's commits touch the last 500.
    while (Date.now() < end) {
      const now = Date.now();
      if (now >= nextEdit) {
        for (let k = 0; k < 50; k++) {
          const i = (counts.edits * 50 + k) % 2000;
          writeFileSync(join(wc, fileName(i)), `file ${i}\nlocal edit ${counts.edits}\n`);
        }
        counts.edits++;
        nextEdit += 15_000;
      }
      if (now >= nextElsewhere) {
        commitElsewhere(repo, counts.commitsElsewhere++);
        nextElsewhere += 60_000;
      }
      if (now >= nextUpdate) {
        nextUpdate += 120_000;
        const rev = counts.commitsElsewhere + 1;
        try {
          await app.click("button", /^Update\b/, 30_000);
          await app.waitText(`Updated to r${rev}`, 110_000);
          counts.updates++;
        } catch (e) {
          counts.failedUpdates++;
          console.log(`soak: update to r${rev} did not finish: ${e.message}`);
        }
      }
      await sleep(500);
    }
    sampler.stop();
    // Still responsive and consistent at the end.
    await app.waitFor(
      async () => /\d+ changed/.test(await app.bodyText()),
      60_000,
      "the changes list",
    );
    for (const [cols] of svnStatus(wc)) assert.ok(/^[M ]/.test(cols), `svn status ${cols}`);

    const soak = sampler.samples.filter((s) => s.phase === "soak");
    const fifth = Math.max(1, Math.floor(soak.length / 5));
    const warm = mean(soak.slice(fifth, 2 * fifth).map((s) => s.rssKb));
    const last = mean(soak.slice(-fifth).map((s) => s.rssKb));
    const growthMb = mb(last - warm);
    const perMinute = [];
    for (let m = 0; m < MINUTES; m++) {
      const xs = soak.filter((s) => s.t >= start + m * 60_000 && s.t < start + (m + 1) * 60_000);
      if (xs.length)
        perMinute.push({
          minute: m + 1,
          rssMb: mb(Math.max(...xs.map((s) => s.rssKb))),
          appRssMb: mb(Math.max(...xs.map((s) => s.appRssKb))),
        });
    }
    const report = {
      date: new Date().toISOString(),
      app: APP,
      appEnv: APP_ENV,
      minutes: MINUTES,
      files: FILES,
      counts,
      peakTreeRssMb: mb(sampler.peak.rssKb),
      peakAppRssMb: mb(sampler.peak.appRssKb),
      warmMeanMb: mb(warm),
      lastMeanMb: mb(last),
      growthMb,
      perMinute,
    };
    writeArtifact("soak-report.json", `${JSON.stringify(report, null, 2)}\n`);
    writeArtifact(
      "soak-report.md",
      [
        `# App soak, ${MINUTES} min, ${FILES} files${Object.keys(APP_ENV).length ? ` (${process.env.OPENSVN_SOAK_APP_ENV})` : ""}`,
        "",
        `Peak tree RSS ${report.peakTreeRssMb} MB (app ${report.peakAppRssMb} MB). Mean RSS after warm-up ${report.warmMeanMb} MB, last fifth ${report.lastMeanMb} MB: growth ${growthMb} MB (limit ${MAX_GROWTH_MB}).`,
        `${counts.edits} edit bursts, ${counts.commitsElsewhere} commits elsewhere, ${counts.updates} updates (${counts.failedUpdates} not finished in time).`,
        "",
        "| Minute | Tree RSS peak | App RSS peak |",
        "|---|---|---|",
        ...perMinute.map((p) => `| ${p.minute} | ${p.rssMb} MB | ${p.appRssMb} MB |`),
        "",
      ].join("\n"),
    );
    console.log(JSON.stringify({ ...report, perMinute: undefined }, null, 2));
    assert.equal(counts.failedUpdates, 0, "every periodic update finished");
    assert.ok(report.peakTreeRssMb < LIMIT_MB, `peak ${report.peakTreeRssMb} MB >= ${LIMIT_MB} MB`);
    assert.ok(growthMb <= MAX_GROWTH_MB, `memory crept by ${growthMb} MB`);
  } catch (e) {
    if (app) await saveFailure(app, "soak");
    throw e;
  } finally {
    sampler?.stop();
    if (app) await app.stop();
    repo.dispose();
  }
});
