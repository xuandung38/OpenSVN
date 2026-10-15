// Memory of the real app on a large working copy (#21): checks out a 100k-file repository through
// the UI, then runs status (reopening the working copy), a status refresh, a watcher burst (5k
// files edited at once), an update of 20 incoming revisions and History. The resident memory of
// the app process tree (the app and its WebKit web/network processes) is sampled every 250 ms;
// the peak must stay under 400 MB.
//
// OPENSVN_RSS_FILES (default 100000), OPENSVN_RSS_BURST (5000) and OPENSVN_RSS_LIMIT_MB (400)
// scale the run; OPENSVN_RSS_APP_ENV adds variables to the app's environment (e.g.
// "WEBKIT_DISABLE_DMABUF_RENDERER=1": under Xvfb the DMA-BUF renderer runs on llvmpipe and keeps
// about 160 MB more in the web process). Run it on a release build (OPENSVN_E2E_APP=target/release/opensvn): a debug
// build is much larger. The numbers go to e2e-artifacts/rss-report.{md,json}.

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
  reopen,
  runOperation,
  saveFailure,
  seedLarge,
  startKeyring,
  svn,
  writeArtifact,
} from "./reliability.mjs";

const FILES = Number(process.env.OPENSVN_RSS_FILES ?? 100_000);
const BURST = Number(process.env.OPENSVN_RSS_BURST ?? 5_000);
const LIMIT_MB = Number(process.env.OPENSVN_RSS_LIMIT_MB ?? 400);
const APP_ENV = appEnvFrom("OPENSVN_RSS_APP_ENV");
const INCOMING = 20;
/** The burst edits every STRIDE-th file. */
const STRIDE = Math.max(2, Math.floor(FILES / BURST));
/** 100 files per folder, like a source tree. */
const PER_DIR = 100;
const fileName = (i) =>
  `m${Math.floor(i / (PER_DIR * 100))}/d${Math.floor(i / PER_DIR) % 100}/f${i}.txt`;

/** @type {{ env: Record<string, string>, stop(): void }} */
let keyring;

before(() => {
  keyring = startKeyring();
});
after(() => keyring?.stop());

/** `INCOMING` revisions from someone else, each changing a few files (svnmucc, no checkout). */
function incoming(repo) {
  for (let r = 0; r < INCOMING; r++) {
    const files = {};
    for (let k = 0; k < 5; k++) {
      // One past a multiple of STRIDE: never a file of the burst, so no conflict.
      const i = (((r * 5 + k) * 997) % Math.floor(FILES / STRIDE)) * STRIDE + 1;
      files[`${repo.url}/trunk/${fileName(i)}`] = `file ${i}\nchanged by r${r}\n`;
    }
    commitUrls(repo, `Incoming ${r}`, files);
  }
}

test(`app RSS on a ${FILES}-file working copy stays under ${LIMIT_MB} MB`, async () => {
  const repo = new ScratchRepo("rss");
  let app;
  let sampler;
  const steps = [];
  let busyRetries = 0;
  const timed = async (phase, fn) => {
    sampler.mark(phase);
    const t0 = Date.now();
    await fn();
    steps.push({ phase, ms: Date.now() - t0 });
  };
  try {
    const seedStart = Date.now();
    seedLarge(repo, FILES, fileName);
    console.log(`rss: seeded ${FILES} files in ${Date.now() - seedStart} ms`);
    app = await App.start("rss", { ...keyring.env, ...APP_ENV });
    sampler = new MemorySampler(app);
    let wc;
    await timed("checkout", async () => {
      wc = await checkout(app, repo, "trunk", 1_800_000);
    });
    const home = app.home;

    // Status: restart, so the working copy is opened and scanned from cold.
    await app.stop({ keepHome: true });
    app = await App.start("rss", { ...keyring.env, ...APP_ENV }, { home });
    sampler.follow(app);
    await timed("status", async () => {
      await reopen(app, repo.name, 600_000);
      await app.waitText("No local changes", 600_000);
    });

    // Opening at start-up (recovery, first scan) runs first: the refresh would find it busy.
    const idle = () =>
      app.waitFor(
        async () => /Activity\s+Idle/.test(await app.bodyText()),
        600_000,
        "the app to be idle",
      );
    await idle();
    const [entry] = (await invoke(app, "wc_list", {})).ok;
    await timed("refresh", async () => {
      // Right after start-up the working copy may still be opening (not an operation, so the
      // Activity item says Idle): the refresh is refused as busy and retryable. Retry like the
      // user would, and count it.
      for (;;) {
        const end = await runOperation(app, "status_refresh", { wcId: entry.id, paths: null });
        if (end.finished) break;
        assert.equal(
          end.failed?.error?.code,
          "wc-busy",
          `the refresh failed: ${JSON.stringify(end.failed)}`,
        );
        assert.ok(++busyRetries <= 20, "the working copy stayed busy");
        await sleep(1000);
      }
    });

    await timed("watcher burst", async () => {
      for (let k = 0; k < BURST; k++) {
        const i = k * STRIDE;
        writeFileSync(join(wc, fileName(i)), `file ${i}\nlocal edit\n`);
      }
      await app.waitText(`${BURST} changed`, 600_000);
    });

    incoming(repo);
    await timed("update", async () => {
      await app.click("button", /^Update\b/);
      await app.waitText(`Updated to r${INCOMING + 1}`, 600_000);
    });

    // The log of the working copy reaches its BASE revision: after the update, the newest one
    // (the list is virtualised: older rows are not rendered).
    await timed("history", async () => {
      await app.click("button", "History");
      await app.waitText(`Incoming ${INCOMING - 1}`, 120_000);
    });
    sampler.stop();
    const status = svn(["status", "-q"], wc);
    assert.equal(status.trim().split("\n").length, BURST, "svn status: the burst edits only");

    const peakMb = mb(sampler.peak.rssKb);
    const report = {
      date: new Date().toISOString(),
      app: APP,
      appEnv: APP_ENV,
      files: FILES,
      burst: BURST,
      refreshBusyRetries: busyRetries,
      limitMb: LIMIT_MB,
      peak: {
        treeRssMb: peakMb,
        treePssMb: mb(sampler.peak.pssKb),
        appRssMb: mb(sampler.peak.appRssKb),
        processes: sampler.peak.pids,
      },
      steps: steps.map((s) => {
        const p = sampler.phasePeaks.get(s.phase);
        return {
          ...s,
          treeRssMb: mb(p?.rssKb ?? 0),
          treePssMb: mb(p?.pssKb ?? 0),
          appRssMb: mb(p?.appRssKb ?? 0),
        };
      }),
    };
    writeArtifact("rss-report.json", `${JSON.stringify(report, null, 2)}\n`);
    writeArtifact(
      "rss-report.md",
      [
        `# App RSS, ${FILES} files${Object.keys(APP_ENV).length ? ` (${process.env.OPENSVN_RSS_APP_ENV})` : ""}`,
        "",
        `Peak of the app process tree: RSS ${peakMb} MB (PSS ${report.peak.treePssMb} MB), app process alone ${report.peak.appRssMb} MB; limit ${LIMIT_MB} MB.`,
        "",
        "| Step | Time | Tree RSS peak | Tree PSS peak | App RSS peak |",
        "|---|---|---|---|---|",
        ...report.steps.map(
          (s) =>
            `| ${s.phase} | ${(s.ms / 1000).toFixed(1)} s | ${s.treeRssMb} MB | ${s.treePssMb} MB | ${s.appRssMb} MB |`,
        ),
        "",
      ].join("\n"),
    );
    console.log(JSON.stringify(report, null, 2));
    assert.ok(peakMb < LIMIT_MB, `peak RSS of the app tree ${peakMb} MB >= ${LIMIT_MB} MB`);
  } catch (e) {
    if (app) await saveFailure(app, "rss");
    throw e;
  } finally {
    sampler?.stop();
    if (app) await app.stop();
    repo.dispose();
  }
});
