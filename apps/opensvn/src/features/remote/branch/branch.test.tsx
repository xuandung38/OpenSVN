import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, expect, test } from "vitest";
import { mockCalls } from "../../../ipc/mock/adapter";
import { MOCK_REPO_ROOT } from "../../../ipc/mock/remote-data";
import { renderApp, setupMockIpc, stubLayout } from "../../../test-utils";
import { parseRevision, projectRootOf, refTargetUrl } from "./branch-model";
import { CreateBranchTagDialog } from "./create-branch-tag-dialog";

beforeAll(async () => {
  stubLayout();
  await setupMockIpc("en");
});

test("projectRootOf finds the part before trunk/branches/tags", () => {
  expect(projectRootOf("https://h/r/trunk")).toBe("https://h/r");
  expect(projectRootOf("https://h/r/ProjectA/branches/x/sub")).toBe("https://h/r/ProjectA");
  expect(projectRootOf("https://h/r/TAGS/v1")).toBe("https://h/r");
  expect(projectRootOf("https://h/r/docs/plan")).toBeNull();
});

test("refTargetUrl and parseRevision", () => {
  expect(refTargetUrl("https://h/r", "branch", "feature a")).toBe(
    "https://h/r/branches/feature%20a",
  );
  expect(refTargetUrl("https://h/r", "tag", "v1")).toBe("https://h/r/tags/v1");
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
      wcId="wc-main"
      onClose={() => undefined}
    />,
  );
  const dialog = await screen.findByRole("dialog", { name: "New branch", hidden: true });
  const create = within(dialog).getByRole("button", { name: "Create" }) as HTMLButtonElement;
  expect(create.disabled).toBe(true);
  await userEvent.type(within(dialog).getByLabelText("Name"), "feature-y");
  expect(within(dialog).getByText("/branches/feature-y")).toBeTruthy();
  await userEvent.click(within(dialog).getByLabelText(/Switch the working copy/));
  expect(create.disabled).toBe(false);
  await userEvent.click(create);
  await waitFor(() => expect(mockCalls.some((c) => c.name === "copyLocation")).toBe(true));
  expect(mockCalls.find((c) => c.name === "copyLocation")?.args[0]).toEqual({
    fromUrl: `${MOCK_REPO_ROOT}/trunk`,
    fromRev: null,
    toUrl: `${MOCK_REPO_ROOT}/branches/feature-y`,
    message: "Create branch feature-y",
    switchWcId: "wc-main",
  });
});

test("tag mode has no switch option; a specific revision must be a number", async () => {
  renderApp(
    <CreateBranchTagDialog
      kind="tag"
      sourceUrl={`${MOCK_REPO_ROOT}/trunk`}
      sourceName="trunk"
      repoRootUrl={MOCK_REPO_ROOT}
      wcId="wc-main"
      onClose={() => undefined}
    />,
  );
  const dialog = await screen.findByRole("dialog", { name: "New tag", hidden: true });
  expect(within(dialog).queryByLabelText(/Switch the working copy/)).toBeNull();
  await userEvent.type(within(dialog).getByLabelText("Name"), "v3");
  await userEvent.click(within(dialog).getByLabelText("Specific revision"));
  await userEvent.type(within(dialog).getByLabelText("Revision"), "abc");
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
  await userEvent.click(within(dialog).getByRole("button", { name: "Create" }));
  await waitFor(() => expect(mockCalls.some((c) => c.name === "copyLocation")).toBe(true));
  expect(mockCalls.find((c) => c.name === "copyLocation")?.args[0]).toMatchObject({
    fromUrl: `${MOCK_REPO_ROOT}/trunk/src`,
    toUrl: `${MOCK_REPO_ROOT}/src-copy`,
  });
});

test("a branch request on a non-layout URL falls back to location mode with an explanation", async () => {
  renderApp(
    <CreateBranchTagDialog
      kind="branch"
      sourceUrl="https://h/r/docs/plan"
      sourceName="plan"
      repoRootUrl="https://h/r"
      wcId={null}
      onClose={() => undefined}
    />,
  );
  const dialog = await screen.findByRole("dialog", { name: "Copy to new location", hidden: true });
  expect(
    within(dialog).getByText(/does not follow the trunk\/branches\/tags convention/),
  ).toBeTruthy();
});
