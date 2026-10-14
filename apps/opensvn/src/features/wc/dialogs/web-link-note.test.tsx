import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, beforeEach, expect, test } from "vitest";
import { mockCalls } from "../../../ipc/mock/adapter";
import type { Lang } from "../../../lib/i18n/i18n";
import { renderWithQuery, setupMockIpc } from "../../../test-utils";
import { CheckoutDialog } from "./checkout-dialog";

beforeAll(async () => {
  await setupMockIpc("en");
});

beforeEach(() => {
  mockCalls.length = 0;
});

async function pasteUrl(text: string, lang: Lang = "en") {
  renderWithQuery(<CheckoutDialog onClose={() => {}} />, lang);
  const box = (await screen.findByRole("combobox", {
    name: lang === "en" ? "Repository URL" : "URL repository",
  })) as HTMLInputElement;
  const user = userEvent.setup();
  await user.click(box);
  await user.paste(text);
  return box;
}

/** The conversion note (the toast region is another, empty, status element). */
const findNote = () =>
  waitFor(() => {
    const n = screen.getAllByRole("status").find((e) => /VisualSVN/.test(e.textContent ?? ""));
    if (!n) throw new Error("no conversion note");
    return n;
  });
const queryNote = () =>
  screen.queryAllByRole("status").find((e) => /VisualSVN/.test(e.textContent ?? ""));

const revisionBox = (name: string) => screen.getByRole("textbox", { name }) as HTMLInputElement;
const checkoutButton = () => screen.getByRole("button", { name: "Checkout" }) as HTMLButtonElement;

test("a VisualSVN web link is replaced by the repository URL with a note", async () => {
  const box = await pasteUrl("https://svn.example.com/!/#game/view/r5/trunk");
  const note = await findNote();
  expect(note.textContent).toBe(
    "Converted VisualSVN web link to repository URL https://svn.example.com/svn/game/trunk Revision 5 from the link was filled in.",
  );
  expect(box.value).toBe("https://svn.example.com/svn/game/trunk");
  expect(revisionBox("Revision").value).toBe("5");
  expect(mockCalls.filter((c) => c.name === "repoResolveUrl").map((c) => c.args[0])).toEqual([
    "https://svn.example.com/!/#game/view/r5/trunk",
  ]);
  await waitFor(() => expect(checkoutButton().disabled).toBe(false));
  // Editing the URL drops the note.
  await userEvent.setup().type(box, "/src");
  expect(queryNote()).toBeUndefined();
});

test("a head link converts without touching the revision; Vietnamese note", async () => {
  const box = await pasteUrl(
    "https://svn.example.com:8443/vsvn/!/#game/view/head/branches/x",
    "vi",
  );
  const note = await findNote();
  expect(note.textContent).toBe(
    "Đã chuyển link web VisualSVN thành URL repository https://svn.example.com:8443/vsvn/svn/game/branches/x",
  );
  expect(box.value).toBe("https://svn.example.com:8443/vsvn/svn/game/branches/x");
  expect(revisionBox("Revision").value).toBe("");
});

test("a web link without a repository behind it explains where to copy the URL", async () => {
  const box = await pasteUrl("https://svn.example.com/!/#Unknown.Repo/view/head/trunk");
  const alert = await screen.findByRole("alert");
  expect(alert.textContent).toContain("You pasted a VisualSVN web-interface link");
  expect(alert.textContent).toContain("“Copy URL to clipboard”");
  expect(box.value).toBe("https://svn.example.com/!/#Unknown.Repo/view/head/trunk");
  expect(checkoutButton().disabled).toBe(true);
});

test("the same failure in Vietnamese", async () => {
  await pasteUrl("https://svn.example.com/!/#khong-co", "vi");
  const alert = await screen.findByRole("alert");
  expect(alert.textContent).toContain("Bạn vừa dán link giao diện web VisualSVN");
  expect(alert.textContent).toContain("“Copy URL to clipboard”");
});

test("other URLs with ? or # say what a repository URL looks like", async () => {
  await pasteUrl("https://svn.example.com/svn/game#readme");
  const alert = await screen.findByRole("alert");
  expect(alert.textContent).toBe(
    "A repository URL has no ? or # part. It looks like https://svn.example.com/svn/Repo/trunk.",
  );
  expect(checkoutButton().disabled).toBe(true);
  expect(mockCalls.some((c) => c.name === "repoResolveUrl")).toBe(false);
});
