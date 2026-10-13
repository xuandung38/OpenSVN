import { screen, waitFor, within } from "@testing-library/react";
import { beforeAll, expect, test, vi } from "vitest";
import { mockCalls } from "../../../ipc/mock/adapter";
import { MOCK_REPO_ROOT, MOCK_REPO_UUID } from "../../../ipc/mock/remote-data";
import { installFakeClock, renderApp, setupMockIpc, stubLayout } from "../../../test-utils";
import { type LayoutContext, parseRevision, projectRootOf, refContainerUrl } from "./branch-model";
import { CreateBranchTagDialog } from "./create-branch-tag-dialog";

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
});

// Fake clock: findBy/waitFor budgets count simulated time, so a loaded machine cannot time them out.
const { user } = installFakeClock();
// The per-test timeout is only a hang guard now: under CPU load rendering alone can take many
// wall-clock seconds, while findBy/waitFor budgets run on the fake clock.
vi.setConfig({ testTimeout: 120_000 });

test("projectRootOf finds the part before trunk/branches/tags", () => {
  expect(projectRootOf("https://h/r/trunk")).toBe("https://h/r");
  expect(projectRootOf("https://h/r/ProjectA/branches/x/sub")).toBe("https://h/r/ProjectA");
  expect(projectRootOf("https://h/r/TAGS/v1")).toBe("https://h/r");
  expect(projectRootOf("https://h/r/docs/plan")).toBeNull();
});

test("parseRevision", () => {
  expect(parseRevision("")).toBeNull();
  expect(parseRevision("head")).toBeNull();
  expect(parseRevision("42")).toBe(42);
  for (const bad of ["0", "-1", "1.5", "r5", "9007199254740993"])
    expect(parseRevision(bad)).toBe("invalid");
});

test("layout mode: new branch copies trunk to branches/<name> with the default message and optional switch", async () => {
  mockCalls.length = 0;
  renderApp(
    <CreateBranchTagDialog
      kind="branch"
      sourceUrl={`${MOCK_REPO_ROOT}/trunk`}
      sourceName="trunk"
      repoRootUrl={MOCK_REPO_ROOT}
      repoUuid={MOCK_REPO_UUID}
      currentRefUrl={`${MOCK_REPO_ROOT}/trunk`}
      wcId="wc-main"
      onClose={() => undefined}
    />,
  );
  const dialog = await screen.findByRole("dialog", { name: "New branch", hidden: true });
  const create = within(dialog).getByRole("button", { name: "Create" }) as HTMLButtonElement;
  expect(create.disabled).toBe(true);
  await user().type(within(dialog).getByLabelText("Name"), "feature-y");
  expect(within(dialog).getByText("/branches/feature-y")).toBeTruthy();
  await user().click(within(dialog).getByLabelText(/Switch the working copy/));
  expect(create.disabled).toBe(false);
  await user().click(create);
  await waitFor(() => expect(mockCalls.some((c) => c.name === "copyLocation")).toBe(true));
  expect(mockCalls.find((c) => c.name === "copyLocation")?.args[0]).toEqual({
    fromUrl: `${MOCK_REPO_ROOT}/trunk`,
    fromRev: null,
    toUrl: `${MOCK_REPO_ROOT}/branches/feature-y`,
    message: "Create branch feature-y",
    switchWcId: "wc-main",
    fromWc: null,
  });
});

test("tag mode has no switch option; a specific revision must be a number", async () => {
  renderApp(
    <CreateBranchTagDialog
      kind="tag"
      sourceUrl={`${MOCK_REPO_ROOT}/trunk`}
      sourceName="trunk"
      repoRootUrl={MOCK_REPO_ROOT}
      repoUuid={MOCK_REPO_UUID}
      currentRefUrl={`${MOCK_REPO_ROOT}/trunk`}
      wcId="wc-main"
      onClose={() => undefined}
    />,
  );
  const dialog = await screen.findByRole("dialog", { name: "New tag", hidden: true });
  expect(within(dialog).queryByLabelText(/Switch the working copy/)).toBeNull();
  await user().type(within(dialog).getByLabelText("Name"), "v3");
  await user().click(within(dialog).getByLabelText("Specific revision"));
  await user().type(within(dialog).getByLabelText("Revision"), "abc");
  expect(within(dialog).getByRole("alert").textContent).toMatch(/revision number/);
  expect(
    (within(dialog).getByRole("button", { name: "Create" }) as HTMLButtonElement).disabled,
  ).toBe(true);
});

test("location mode (no layout): copy to a chosen folder, source wc copy is not offered", async () => {
  mockCalls.length = 0;
  renderApp(
    <CreateBranchTagDialog
      kind="location"
      sourceUrl={`${MOCK_REPO_ROOT}/trunk/src`}
      sourceName="src"
      repoRootUrl={MOCK_REPO_ROOT}
      repoUuid={MOCK_REPO_UUID}
      currentRefUrl={`${MOCK_REPO_ROOT}/trunk`}
      wcId={null}
      onClose={() => undefined}
    />,
  );
  const dialog = await screen.findByRole("dialog", { name: "Copy to new location", hidden: true });
  expect(within(dialog).queryByLabelText(/Switch/)).toBeNull();
  expect(
    (within(dialog).getByLabelText(/Working copy \(includes local changes\)/) as HTMLInputElement)
      .disabled,
  ).toBe(true);
  // Default destination folder is the repo root, name defaults to "<source>-copy".
  expect((within(dialog).getByLabelText("New name") as HTMLInputElement).value).toBe("src-copy");
  await user().click(within(dialog).getByRole("button", { name: "Create" }));
  await waitFor(() => expect(mockCalls.some((c) => c.name === "copyLocation")).toBe(true));
  expect(mockCalls.find((c) => c.name === "copyLocation")?.args[0]).toMatchObject({
    fromUrl: `${MOCK_REPO_ROOT}/trunk/src`,
    toUrl: `${MOCK_REPO_ROOT}/src-copy`,
  });
});

const ROOT = "https://h/r";
const ctx = (over: Partial<LayoutContext>): LayoutContext => ({
  layout: null,
  nodes: [],
  currentRefUrl: null,
  sourceUrl: `${ROOT}/docs/plan`,
  repoRootUrl: ROOT,
  ...over,
});

test("refContainerUrl prefers backend nodes, then the configured layout, then the naming convention", () => {
  // Existing branch node: its parent folder is the container, whatever it is called.
  expect(
    refContainerUrl(
      "branch",
      ctx({ nodes: [{ kind: "branch", name: "x", url: `${ROOT}/Dev/Forks/x` }] }),
    ),
  ).toBe(`${ROOT}/Dev/Forks`);
  // Custom layout without any branch yet.
  const layout = {
    mode: "custom" as const,
    trunk: "/{project}/Main",
    branches: ["/{project}/Forks"],
    tags: ["/{project}/Releases"],
  };
  expect(refContainerUrl("branch", ctx({ layout, currentRefUrl: `${ROOT}/Game/Main` }))).toBe(
    `${ROOT}/Game/Forks`,
  );
  expect(refContainerUrl("tag", ctx({ layout, currentRefUrl: `${ROOT}/Game/Forks/x` }))).toBe(
    `${ROOT}/Game/Releases`,
  );
  // Fixed (no {project}) custom pattern.
  expect(
    refContainerUrl(
      "branch",
      ctx({ layout: { mode: "custom", trunk: "/Main", branches: ["/Forks"], tags: ["/Rel"] } }),
    ),
  ).toBe(`${ROOT}/Forks`);
  // Cannot tell the project: no destination, caller falls back to Location mode.
  expect(refContainerUrl("branch", ctx({ layout }))).toBeNull();
  // Layout "none" never yields a container, even if the URL looks conventional.
  expect(
    refContainerUrl("branch", ctx({ layout: { mode: "none" }, sourceUrl: `${ROOT}/trunk` })),
  ).toBeNull();
  // Auto layout, nothing known yet: naming convention fallback.
  expect(refContainerUrl("branch", ctx({ sourceUrl: `${ROOT}/P/trunk` }))).toBe(
    `${ROOT}/P/branches`,
  );
  expect(refContainerUrl("tag", ctx({ sourceUrl: `${ROOT}/docs/plan` }))).toBeNull();
});

test("multi-project repo: only the current project's branches and tags are considered", () => {
  const nodes = [
    { kind: "branch" as const, name: "b1", url: `${ROOT}/ProjectB/branches/b1` },
    { kind: "tag" as const, name: "t1", url: `${ROOT}/ProjectB/tags/t1` },
    { kind: "branch" as const, name: "a1", url: `${ROOT}/ProjectA/branches/a1` },
  ];
  // Working copy at /ProjectA/trunk: copies go to ProjectA, never to the first project listed.
  const a = ctx({
    nodes,
    currentRefUrl: `${ROOT}/ProjectA/trunk`,
    sourceUrl: `${ROOT}/ProjectA/trunk`,
  });
  expect(refContainerUrl("branch", a)).toBe(`${ROOT}/ProjectA/branches`);
  // ProjectA has no tag yet and ProjectB's tag must not be used: convention for ProjectA.
  expect(refContainerUrl("tag", a)).toBe(`${ROOT}/ProjectA/tags`);
  const b = ctx({
    nodes,
    currentRefUrl: `${ROOT}/ProjectB/trunk`,
    sourceUrl: `${ROOT}/ProjectB/trunk`,
  });
  expect(refContainerUrl("tag", b)).toBe(`${ROOT}/ProjectB/tags`);
});

test("multi-project repo with a custom layout uses the {project} of the current ref", () => {
  const layout = {
    mode: "custom" as const,
    trunk: "/{project}/Main",
    branches: ["/{project}/Forks"],
    tags: ["/{project}/Releases"],
  };
  const nodes = [{ kind: "branch" as const, name: "x", url: `${ROOT}/Other/Forks/x` }];
  // Current project (Game) has no branch yet: the other project's branch is ignored.
  expect(
    refContainerUrl("branch", ctx({ layout, nodes, currentRefUrl: `${ROOT}/Game/Main` })),
  ).toBe(`${ROOT}/Game/Forks`);
  expect(
    refContainerUrl("branch", ctx({ layout, nodes, currentRefUrl: `${ROOT}/Other/Main` })),
  ).toBe(`${ROOT}/Other/Forks`);
});

test("unknown project with nodes in several folders is ambiguous, not guessed", () => {
  const nodes = [
    { kind: "branch" as const, name: "a", url: `${ROOT}/A/Forks/a` },
    { kind: "branch" as const, name: "b", url: `${ROOT}/B/Forks/b` },
  ];
  expect(refContainerUrl("branch", ctx({ nodes }))).toBeNull();
});
