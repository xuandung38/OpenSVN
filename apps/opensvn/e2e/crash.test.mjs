// App-level crash recovery (#20, #21): the real app (real engine, `failpoints` build) commits or
// updates against Apache + mod_dav_svn and is killed with SIGKILL in the middle, together with its
// whole process group, at a random moment or when a named failpoint is reached. It is then
// relaunched on the same profile and must recover on its own:
// - no "Working copy is locked" / "Run cleanup" banner;
// - `svn status` (the oracle client, svn 1.15) reads the working copy and shows nothing but the
//   expected local edits, every local edit still on disk;
// - a commit lands at most once: after the user finishes it (commits again when it did not land)
//   the repository holds exactly one new revision with every change;
// - an interrupted update finishes (Continue update or Update) and merges the incoming change
//   with the local edits.
//
// OPENSVN_CRASH_SEED replays a run (printed at the start and in the report);
// OPENSVN_CRASH_ITERS kills per operation (default 4, half random, half at a failpoint);
// OPENSVN_CRASH_FILES files in the repository (default 240); OPENSVN_CRASH_PLAN=<failpoint>:<hit>
// replaces the failpoint of every failpoint case (replays one case of a report). The report goes to
// e2e-artifacts/crash-report.md. See e2e/README.md.

import assert from "node:assert/strict";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { after, before, test } from "node:test";
import { App, ScratchRepo, sleep } from "./harness.mjs";
import {
  checkout,
  REPAIR_BANNERS,
  reopen,
  repairBanners,
  rng,
  saveFailure,
  startKeyring,
  svn,
  svnStatus,
  waitForFile,
  writeArtifact,
  youngest,
} from "./reliability.mjs";
import { Keys } from "./webdriver.mjs";

const SEED = Number(process.env.OPENSVN_CRASH_SEED ?? Date.now() % 2 ** 31);
const ITERS = Number(process.env.OPENSVN_CRASH_ITERS ?? 4);
const FILES = Number(process.env.OPENSVN_CRASH_FILES ?? 240);
const BODY = "x".repeat(16_000);

/** Failpoints a commit passes through, in order (svn-client commit + the working copy layer). */
const COMMIT_POINTS = [
  "commit.after_txn_created",
  "commit.after_merge_sent_marker",
  "commit.after_merge_before_finalize",
  "commit.journal_merged_write",
  "commit.finalize",
  "commit.after_finalize_before_queue",
  "wc.wq.after_fileinfo",
];
/** Failpoints of an update without conflicts; most are hit once per file. */
const UPDATE_POINTS = [
  "update.after_merge3_before_txn",
  "update.after_txn_before_queue",
  "pristine.install.before_rename",
  "wc.wq.after_fileinfo",
  "wc.txn.after_commit",
];

/** @type {{ env: Record<string, string>, stop(): void }} */
let keyring;
const rows = [];
/** Duration of an uninterrupted run of each operation (the random kill window). */
const control = { commit: 0, update: 0 };

before(() => {
  console.log(`crash e2e: seed ${SEED}, ${ITERS} kills per operation, ${FILES} files`);
  keyring = startKeyring();
});

after(() => {
  keyring?.stop();
  const lines = [
    "# App crash e2e",
    "",
    `Seed ${SEED}, ${FILES} files, control: commit ${control.commit} ms, update ${control.update} ms.`,
    "",
    "| # | Operation | Kill | Killed after | Landed | Repair banner | svn status after relaunch | Finished |",
    "|---|---|---|---|---|---|---|---|",
    ...rows.map(
      (r, i) =>
        `| ${i + 1} | ${r.op} | ${r.kill} | ${r.killedMs ?? "-"} ms | ${r.landed ?? "-"} | ${r.banner === "" ? "none" : r.banner} | ${r.status} | ${r.finished} |`,
    ),
    "",
  ];
  writeArtifact("crash-report.md", lines.join("\n"));
});

/** `count` text files in 8 folders; every file has a first and last line tests can change. */
function fixture(count) {
  const files = {};
  for (let i = 0; i < count; i++) files[name(i)] = text(i, `first ${i}`, `last ${i}`);
  return files;
}
const name = (i) => `d${i % 8}/f${i}.txt`;
const text = (i, first, last) => `${first}\n${i}\n${BODY}\n${last}\n`;

/** How this case kills the app: a control run, a random moment, or at a failpoint hit. */
function plan(op, iter) {
  // One random source per case: a filtered run (--test-name-pattern) replays the same cases.
  const random = rng(SEED + iter * 7919 + (op === "update" ? 104_729 : 0));
  if (iter === 0) return { kind: "none" };
  if (iter % 2 === 1) {
    const forced = process.env.OPENSVN_CRASH_PLAN?.match(/^(.+):(\d+)$/);
    if (forced) return { kind: "failpoint", point: forced[1], hit: Number(forced[2]) };
    const points = op === "commit" ? COMMIT_POINTS : UPDATE_POINTS;
    const point = random.pick(points);
    // Per-file points are hit once per changed file: pick a hit inside the operation.
    const perFile = /after_txn_before_queue|merge3|pristine|wc\./.test(point);
    const hit = perFile ? 1 + random.int(20) : 1;
    return { kind: "failpoint", point, hit };
  }
  const window = Math.max(control[op], 500) * 1.1;
  return { kind: "random", delay: Math.round(random.next() * window) };
}

/** Starts the app on `home`, armed with the failpoint of `how` when it has one. */
async function launch(home, how, marker) {
  const env = { ...keyring.env };
  if (how?.kind === "failpoint") {
    env.OPENSVN_CRASH_FAILPOINT = `${how.point}:${how.hit}`;
    env.OPENSVN_CRASH_MARKER = marker;
  }
  return App.start("crash", env, { home });
}

/** Kills the app per `how` once the operation started at `t0`; returns what happened. */
async function killPerPlan(app, how, marker, done, t0) {
  if (how.kind === "none") return { killedMs: null, reached: false };
  if (how.kind === "failpoint") {
    const reached = await waitForFile(() => existsSync(marker) || done(), 120_000);
    const hit = existsSync(marker);
    if (!reached || !hit) return { killedMs: null, reached: false };
    await app.kill();
    return { killedMs: Date.now() - t0, reached: true };
  }
  await sleep(Math.max(0, how.delay - (Date.now() - t0)));
  const finished = done();
  await app.kill();
  return { killedMs: Date.now() - t0, reached: !finished };
}

/** Watches the window text for `re` in the background (the kill can come at any time). */
function watchText(app, re) {
  const state = { seen: false, stop: false };
  (async () => {
    while (!state.stop && !state.seen) {
      try {
        state.seen = re.test(await app.bodyText());
      } catch {
        return; // the app is gone
      }
      await sleep(50);
    }
  })();
  return state;
}

/**
 * After the relaunch: the working copy is current and settled, no repair banner shows up (it is
 * watched for a few seconds, recovery runs while the working copy opens).
 */
async function settleAfterRelaunch(app, repo) {
  await reopen(app, repo.name);
  await app.waitFor(
    async () => /\d+ changed|No local changes/.test(await app.bodyText()),
    60_000,
    "the status of the reopened working copy",
  );
  const deadline = Date.now() + 4_000;
  while (Date.now() < deadline) {
    const banners = await repairBanners(app);
    if (banners.length) return banners.join(", ");
    await sleep(200);
  }
  return "";
}

/** `svn status` must list exactly `expected` (paths) as modified, nothing else, no lock. */
function assertStatus(wc, expected, when) {
  const lines = svnStatus(wc);
  for (const [cols, path] of lines) {
    assert.notEqual(cols[2], "L", `${when}: ${path} is locked:\n${cols}${path}`);
    assert.equal(cols[0], "M", `${when}: unexpected status ${JSON.stringify(cols)} ${path}`);
  }
  const got = lines.map(([, p]) => p).sort();
  assert.deepEqual(got, [...expected].sort(), `${when}: svn status`);
  return lines.length ? `${lines.length} M` : "clean";
}

for (const op of ["commit", "update"]) {
  for (let iter = 0; iter <= ITERS; iter++) {
    test(`${op} #${iter}: SIGKILL, relaunch, automatic recovery`, async (t) => {
      const how = plan(op, iter);
      const label =
        how.kind === "none"
          ? "control"
          : how.kind === "random"
            ? `random @${how.delay} ms`
            : `${how.point}#${how.hit}`;
      t.diagnostic(`seed ${SEED}: ${label}`);
      const row = { op, kill: label, banner: "-", status: "-", finished: "no" };
      rows.push(row);
      const repo = new ScratchRepo(`crash-${op}`);
      let app;
      try {
        repo.init(fixture(FILES));
        app = await launch(undefined, null);
        const home = app.home;
        const marker = join(home, "failpoint-reached");
        const wc = await checkout(app, repo);

        // Edits: commit sends 40 files; update keeps first-line edits under an incoming change.
        const edited = [];
        for (let i = 0; i < FILES; i += op === "commit" ? 6 : 7) {
          edited.push(name(i));
          writeFileSync(join(wc, name(i)), text(i, `local ${i}`, `last ${i}`));
        }
        if (op === "update") {
          const incoming = {};
          for (let i = 0; i < FILES; i += 3)
            incoming[name(i)] = text(i, `first ${i}`, `server ${i}`);
          for (let i = 0; i < 20; i++) incoming[`new/n${i}.txt`] = `new ${i}\n${BODY}\n`;
          repo.commitFromElsewhere(incoming, "Change from elsewhere");
        }
        await app.waitText(`${edited.length} changed`, 30_000);

        // A failpoint is armed at launch: restart on the same profile with it.
        if (how.kind === "failpoint") {
          await app.stop({ keepHome: true });
          app = await launch(home, how, marker);
          await reopen(app, repo.name);
          await app.waitText(`${edited.length} changed`, 30_000);
        }

        const message = `crash ${op} ${SEED}-${iter}`;
        const t0 = Date.now();
        let finishedText;
        if (op === "commit") {
          await app.fill("Summary (required)", message);
          finishedText = watchText(app, /Committed r2/);
          await app.keys(Keys.Control, Keys.Enter);
        } else {
          finishedText = watchText(app, /Updated to r2/);
          await app.click("button", /^Update\b/);
        }
        const killed = await killPerPlan(app, how, marker, () => finishedText.seen, t0);
        if (how.kind === "none") {
          await app.waitFor(() => finishedText.seen, 120_000, `${op} to finish`);
          control[op] = Date.now() - t0;
          await app.stop({ keepHome: true });
        } else if (killed.killedMs === null) {
          // The operation finished before the failpoint hit: still a clean stop and relaunch.
          finishedText.stop = true;
          await app.stop({ keepHome: true });
        }
        finishedText.stop = true;
        row.killedMs = killed.killedMs;
        if (how.kind !== "none" && !killed.reached) row.kill += " (finished first)";

        // Relaunch on the same profile: recovery is automatic.
        app = await launch(home, null);
        row.banner = await settleAfterRelaunch(app, repo);
        assert.equal(row.banner, "", `no repair banner (${REPAIR_BANNERS.join(" / ")})`);

        if (op === "commit") {
          const rev = youngest(repo.url);
          assert.ok(rev === 1 || rev === 2, `the commit landed at most once (youngest r${rev})`);
          row.landed = rev === 2 ? "yes" : "no";
          // Landed: recovery folded it into the working copy. Not landed: every edit is local.
          row.status = assertStatus(wc, rev === 2 ? [] : edited, "after relaunch");
          for (const p of edited) {
            const i = Number(p.match(/f(\d+)/)[1]);
            assert.equal(readFileSync(join(wc, p), "utf8"), text(i, `local ${i}`, `last ${i}`));
          }
          if (rev === 1) {
            await app.fill("Summary (required)", message);
            await app.keys(Keys.Control, Keys.Enter);
            await app.waitText("Committed r2", 60_000);
          }
          await app.waitText("No local changes", 30_000);
          assert.equal(youngest(repo.url), 2, "exactly one new revision");
          const log = svn(["log", "-q", "-v", "-r", "2", repo.url]);
          assert.equal((log.match(/^ {3}M /gm) ?? []).length, edited.length, "r2 has every edit");
          assert.match(svn(["log", "-r", "2", repo.url]), new RegExp(message));
          assertStatus(wc, [], "after finishing");
          const p = edited[edited.length - 1];
          assert.equal(svn(["cat", `${repo.url}/trunk/${p}`]), readFileSync(join(wc, p), "utf8"));
        } else {
          // Readable by svn, every local edit still there (merged or not yet updated).
          const lines = svnStatus(wc);
          for (const [cols, path] of lines) {
            assert.notEqual(cols[2], "L", `${path} is locked`);
            assert.ok(!cols.startsWith("C"), `no conflict: ${cols}${path}`);
          }
          row.status = lines.length ? `${lines.length} entries` : "clean";
          for (const p of edited) {
            assert.match(readFileSync(join(wc, p), "utf8"), /^local \d+\n/, `${p} keeps its edit`);
          }
          // The user finishes the update: Continue update when offered, else Update.
          const body = await app.bodyText();
          if (body.includes("Continue update")) await app.click("button", "Continue update");
          else await app.click("button", /^Update\b/);
          await app.waitText("Updated to r2", 120_000);
          await app.waitFor(
            async () => !/in progress…/.test(await app.bodyText()),
            30_000,
            "the update to end",
          );
          assertStatus(wc, edited, "after finishing");
          assert.equal(svn(["info", "--show-item", "revision"], wc).trim(), "2");
          for (let i = 0; i < FILES; i++) {
            const local = i % 7 === 0 ? `local ${i}` : `first ${i}`;
            const last = i % 3 === 0 ? `server ${i}` : `last ${i}`;
            assert.equal(readFileSync(join(wc, name(i)), "utf8"), text(i, local, last), name(i));
          }
          assert.ok(existsSync(join(wc, "new/n19.txt")));
          // Nothing out of date left: every node is at r2.
          assert.doesNotMatch(svn(["status", "-u", "-q"], wc), /^.{8}\*/m);
        }
        row.finished = "yes";
      } catch (e) {
        row.finished = `no: ${String(e.message).split("\n")[0].replace(/\|/g, "/")}`;
        if (app) await saveFailure(app, `crash-${op}-${iter}`);
        throw e;
      } finally {
        if (app) await app.stop();
        repo.dispose();
      }
    });
  }
}
