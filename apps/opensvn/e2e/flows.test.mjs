// End-to-end flows 10.1-10.5 of docs/system-design/01 (section 10) on the real app, the real
// engine and Apache + mod_dav_svn. See e2e/README.md for how to run them.

import assert from "node:assert/strict";
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, beforeEach, test } from "node:test";
import { App, PASSWORD, ScratchRepo, USER } from "./harness.mjs";
import { Keys } from "./webdriver.mjs";

const ARTIFACTS = process.env.OPENSVN_E2E_ARTIFACTS ?? join(process.cwd(), "e2e-artifacts");

/** @type {App} */
let app;
/** @type {ScratchRepo} */
let repo;

beforeEach(async (t) => {
  repo = new ScratchRepo(t.name.replace(/[^a-z0-9]+/gi, "-").slice(0, 20));
  repo.init({
    "README.md": "hello\n",
    "src/main.c": "int main(void) {\n  return 0;\n}\n",
    "notes.txt": "one\ntwo\nthree\n",
  });
  app = await App.start("flow");
});

afterEach(async (t) => {
  if (t.passed === false || process.exitCode) {
    mkdirSync(ARTIFACTS, { recursive: true });
    const base = join(ARTIFACTS, t.name.replace(/[^a-z0-9]+/gi, "-"));
    try {
      await app.screenshot(`${base}.png`);
      writeFileSync(`${base}.txt`, `${await app.bodyText()}\n\n--- driver log ---\n${app.log()}`);
    } catch {
      // the app may be gone
    }
  }
  await app.stop();
  repo.dispose();
});

/** Working copy folder the checkout created (the default parent is ~/svn). */
function wcRoot() {
  const parent = join(app.home, "svn");
  const [name] = readdirSync(parent);
  return join(parent, name);
}

/** Flow 10.1: Welcome -> Checkout -> URL -> trust certificate -> sign in -> Changes (empty). */
async function checkout(path = "trunk") {
  await app.click("button", "Checkout repository…");
  await app.fill("Repository URL", `${repo.url}/${path}`);
  await app.click("button", "Checkout");
  await app.click("button", "Trust permanently", 30_000);
  await app.role("textbox", "Username", 30_000);
  await app.fill("Username", USER);
  await app.fill("Password", PASSWORD);
  await app.click("button", "Sign in");
  await app.waitText(`Checked out ${repo.name}`, 60_000);
  await app.waitText("No local changes");
  return wcRoot();
}

/** The toolbar's Update button. */
async function update() {
  await app.click("button", /^Update\b/);
}

test("10.1 first use: checkout with certificate trust and sign-in opens an empty Changes list", async () => {
  const wc = await checkout();
  assert.equal(readFileSync(join(wc, "README.md"), "utf8"), "hello\n");
  assert.ok(existsSync(join(wc, ".svn", "wc.db")));
  const text = await app.bodyText();
  assert.match(text, new RegExp(`Current repository\\s+${repo.name}`));
  // trunk/branches/tags layout: the toolbar shows the branch, not the in-repo path (#129).
  assert.match(text, /Branch\s+trunk/);
  assert.match(text, /0 changed/);
});

test("10.2 daily commit: edit, see the diff, commit with Mod+Enter, list empties", async () => {
  const wc = await checkout();
  writeFileSync(join(wc, "README.md"), "hello\nedited by e2e\n");
  // The watcher reports the change without any user action.
  await app.click("option", /README\.md/, 10_000);
  await app.waitText("edited by e2e");
  await app.fill("Summary (required)", "Edit README from e2e");
  await app.keys(Keys.Control, Keys.Enter);
  await app.waitText("Committed r2", 30_000);
  await app.waitText("No local changes");
  assert.equal(repo.svn(["cat", `${repo.url}/trunk/README.md`]), "hello\nedited by e2e\n");
  assert.match(repo.svn(["log", "-r", "2", `${repo.url}`]), /Edit README from e2e/);
});

test("10.2 out of date: the commit is refused, Update merges, the commit goes through", async () => {
  const wc = await checkout();
  repo.commitFromElsewhere({ "notes.txt": "one\ntwo\nthree\nfour from elsewhere\n" });
  writeFileSync(join(wc, "notes.txt"), "zero\none\ntwo\nthree\n");
  await app.click("option", /notes\.txt/, 10_000);
  await app.fill("Summary (required)", "Prepend zero");
  await app.keys(Keys.Control, Keys.Enter);
  await app.waitText("out of date", 30_000);
  await update();
  await app.waitText("Updated to r2", 30_000);
  assert.equal(
    readFileSync(join(wc, "notes.txt"), "utf8"),
    "zero\none\ntwo\nthree\nfour from elsewhere\n",
  );
  await app.fill("Summary (required)", "Prepend zero");
  await app.keys(Keys.Control, Keys.Enter);
  await app.waitText("Committed r3", 30_000);
  assert.equal(
    repo.svn(["cat", `${repo.url}/trunk/notes.txt`]),
    "zero\none\ntwo\nthree\nfour from elsewhere\n",
  );
});

test("10.3 update with a conflict: banner, resolver, mark resolved, banner gone", async () => {
  const wc = await checkout();
  repo.commitFromElsewhere({ "notes.txt": "one\nTWO from elsewhere\nthree\n" });
  writeFileSync(join(wc, "notes.txt"), "one\ntwo from here\nthree\n");
  await app.waitText("1 changed", 10_000);
  await update();
  await app.waitText("conflict", 30_000);
  await app.click("button", "Resolve…");
  await app.role("dialog", /.+/);
  await app.click("radio", /^Mine/);
  await app.click("button", "Resolve");
  await app.waitFor(
    async () => !/Files with conflicts cannot be committed/.test(await app.bodyText()),
    15_000,
    "conflict banner to disappear",
  );
  assert.equal(readFileSync(join(wc, "notes.txt"), "utf8"), "one\ntwo from here\nthree\n");
  assert.ok(!existsSync(join(wc, "notes.txt.mine")), "conflict marker files are removed");
});

test("10.4 needs-lock file: lock to edit, unlock, lock again, commit releases the lock", async () => {
  repo.svn(["propset", "-q", "svn:needs-lock", "*", "README.md"], repo.seed);
  repo.svn(["commit", "-q", "-m", "README needs a lock"], repo.seed);
  const wc = await checkout();
  const readme = join(wc, "README.md");
  const writable = () => (statSync(readme).mode & 0o222) !== 0;
  assert.ok(!writable(), "read-only until locked");
  // The unmodified needs-lock file is listed under Locks (#128): lock it before editing.
  await app.waitText("Locks (1)", 30_000);
  await app.waitText("Needs lock", 30_000);

  await app.click("button", "Lock to edit…");
  await app.role("dialog", "Lock to edit");
  await app.fill("Comment (optional)", "e2e edit");
  await app.click("button", /^Lock$/);
  await app.waitFor(async () => writable(), 30_000, "README.md writable after lock");
  assert.match(repo.svn(["info", `${repo.url}/trunk/README.md`]), /e2e edit/);

  // Still unmodified: Unlock… is reachable from the Locks section.
  await app.click("button", "Unlock…");
  await app.role("dialog", "Unlock");
  await app.click("button", /^Unlock$/);
  await app.waitFor(async () => !writable(), 30_000, "README.md read-only after unlock");
  assert.doesNotMatch(repo.svn(["info", `${repo.url}/trunk/README.md`]), /Lock Token/);

  await app.click("button", "Lock to edit…");
  await app.click("button", /^Lock$/);
  await app.waitFor(async () => writable(), 30_000, "README.md writable after second lock");
  writeFileSync(readme, "hello\nlocked edit\n");
  await app.waitText("1 changed", 30_000);
  await app.fill("Summary (required)", "Edit the locked README");
  await app.keys(Keys.Control, Keys.Enter);
  await app.waitText("Committed r3", 60_000);
  await app.waitFor(async () => !writable(), 30_000, "commit releases the lock");
  assert.doesNotMatch(repo.svn(["info", `${repo.url}/trunk/README.md`]), /Lock Token/);
  assert.equal(repo.svn(["cat", `${repo.url}/trunk/README.md`]), "hello\nlocked edit\n");
});

test("10.4 lock held elsewhere: the lock is refused with the owner, steal needs two confirmations", async () => {
  repo.svn(["propset", "-q", "svn:needs-lock", "*", "README.md"], repo.seed);
  repo.svn(["commit", "-q", "-m", "README needs a lock"], repo.seed);
  repo.svn(["lock", "-q", "-m", "held by the other checkout", "README.md"], repo.seed);
  const wc = await checkout();
  const writable = () => (statSync(join(wc, "README.md")).mode & 0o222) !== 0;
  await app.waitText("Locks (1)", 30_000);
  await app.click("button", "Lock to edit…");
  await app.role("dialog", "Lock to edit");
  await app.click("button", /^Lock$/);
  // The server refuses; the dialog stays open, names the owner and offers to steal.
  await app.waitText(`README.md: locked by ${USER}`, 30_000);
  await app.waitText("Their comment: held by the other checkout", 30_000);
  const steal = await app.role("checkbox", `Steal the lock held by ${USER}`, 30_000);
  assert.ok(!writable(), "nothing was stolen yet");
  await app.s.click(steal["element-6066-11e4-a52e-4f735466cecf"]);
  assert.equal(
    await app.exec("return arguments[0].disabled", await app.role("button", "Steal lock")),
    true,
    "stealing needs the second confirmation",
  );
  await app.click("checkbox", "I understand and want to steal this lock");
  await app.click("button", "Steal lock");
  await app.waitFor(async () => writable(), 30_000, "README.md writable after the steal");
  assert.doesNotMatch(
    repo.svn(["info", `${repo.url}/trunk/README.md`]),
    /held by the other checkout/,
  );
});

test("10.5 switch to a branch through the Branch picker, carrying a local change", async () => {
  repo.svn([
    "copy",
    "-q",
    "-m",
    "Branch feature-x",
    `${repo.url}/trunk`,
    `${repo.url}/branches/feature-x`,
  ]);
  repo.svn(["switch", "-q", `${repo.url}/branches/feature-x`], repo.seed);
  repo.commitFromElsewhere({ "src/main.c": "int main(void) {\n  return 1;\n}\n" }, "Feature work");
  const wc = await checkout();
  writeFileSync(join(wc, "notes.txt"), "one\ntwo\nthree\nlocal\n");
  await app.waitText("1 changed", 30_000);

  // The repository follows trunk/branches/tags: the toolbar shows "Branch trunk" and the popover
  // lists the branches; with a local change it asks before carrying it over.
  await app.click("button", /Branch\s*trunk/, 30_000);
  await app.click("button", /^feature-x/, 30_000);
  await app.waitText("1 local change(s) will be carried over to feature-x");
  await app.click("button", "Carry changes and switch");
  await app.waitFor(
    async () => readFileSync(join(wc, "src/main.c"), "utf8").includes("return 1"),
    60_000,
    "branch content in the working copy",
  );
  assert.equal(readFileSync(join(wc, "notes.txt"), "utf8"), "one\ntwo\nthree\nlocal\n");
  assert.match(repo.svn(["info", "--show-item", "url", wc]), /\/branches\/feature-x$/m);
  await app.role("button", /Branch\s*feature-x/, 30_000);
  await app.waitText("1 changed");

  // The URL field stays available for any location of the repository.
  await app.click("button", /Branch\s*feature-x/);
  await app.click("button", "Switch…");
  await app.fill("URL in the same repository", `${repo.url}/trunk`);
  await app.click("button", /^Switch$/);
  await app.role("button", /Branch\s*trunk/, 60_000);
  assert.match(repo.svn(["info", "--show-item", "url", wc]), /\/trunk$/m);
  assert.equal(readFileSync(join(wc, "notes.txt"), "utf8"), "one\ntwo\nthree\nlocal\n");
});

test("history and blame of a file", async () => {
  repo.commitFromElsewhere({ "notes.txt": "one\ntwo\nthree\nfour\n" }, "Add four");
  await checkout();
  await app.click("tab", "History");
  await app.waitText("Add four", 30_000);
  await app.waitText("Initial import");

  await app.click("tab", "Browse");
  await app.click("option", /notes\.txt/, 30_000);
  await app.click("tab", "Blame");
  // Lines 1-3 come from r1, line 4 from r2, both by the account that signed in.
  await app.waitFor(
    async () =>
      /r1\s+tester[\s\S]*1\s+one[\s\S]*3\s+three\s+r2\s+tester[\s\S]*4\s+four/.test(
        await app.bodyText(),
      ),
    30_000,
    "blame of notes.txt",
  );
});
