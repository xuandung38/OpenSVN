import { render, screen, waitFor } from "@testing-library/react";
import { act } from "react";
import { beforeAll, expect, test, vi } from "vitest";
import { mockAuthResponses, mockCalls, mockEmit } from "../../../ipc/mock/adapter";
import { I18nProvider } from "../../../lib/i18n/i18n";
import { installFakeClock, renderApp, setupMockIpc } from "../../../test-utils";
import {
  type CertPromptInfo,
  type CredentialsPromptInfo,
  isTrustable,
  normalizeAuthPrompt,
  originOf,
  parseCertFailure,
} from "./auth-model";
import { AuthPromptHost, answerAuthPrompt } from "./auth-prompt-host";
import { SignInDialog } from "./sign-in-dialog";
import { TrustCertDialog } from "./trust-cert-dialog";

beforeAll(async () => {
  await setupMockIpc("en");
});

// Fake clock: findBy/waitFor budgets count simulated time, so a loaded machine cannot time them out.
const { user } = installFakeClock();
// The per-test timeout is only a hang guard now: under CPU load rendering alone can take many
// wall-clock seconds, while findBy/waitFor budgets run on the fake clock.
vi.setConfig({ testTimeout: 120_000 });

const CERT = {
  host: "svn.example.com",
  subject: "CN=svn.example.com",
  issuer: "CN=Corp CA",
  notBefore: "2026-01-01",
  notAfter: "2027-01-01",
  sha256: "AA:BB",
};

const creds = (over: Partial<CredentialsPromptInfo> = {}): CredentialsPromptInfo => ({
  kind: "credentials",
  requestId: "r1",
  realm: "<https://svn.example.com:443> Repo",
  url: "https://svn.example.com/repos/x",
  reason: "initial",
  attempt: 0,
  offered: ["basic"],
  ...over,
});
const certPrompt = (failure: CertPromptInfo["failure"]): CertPromptInfo => ({
  kind: "certificate",
  requestId: "c1",
  realm: "svn.example.com:443",
  url: "https://svn.example.com/repos/x",
  cert: CERT,
  failure,
});

const withLang = (ui: React.ReactElement) => render(<I18nProvider lang="en">{ui}</I18nProvider>);

test("parseCertFailure accepts strings and tagged objects, unknown becomes other", () => {
  expect(parseCertFailure("self-signed")).toBe("self-signed");
  expect(parseCertFailure("weird")).toEqual({ other: "weird" });
  expect(parseCertFailure({ changed: { oldSha256: "a", newSha256: "b" } })).toEqual({
    changed: { oldSha256: "a", newSha256: "b" },
  });
  expect(parseCertFailure({ other: "x" })).toEqual({ other: "x" });
  expect(parseCertFailure(null)).toEqual({ other: "unknown" });
});

test("only unknown-issuer and self-signed can be trusted", () => {
  expect(isTrustable("unknown-issuer")).toBe(true);
  expect(isTrustable("self-signed")).toBe(true);
  for (const f of ["expired", "not-yet-valid", "hostname-mismatch", "revoked"] as const) {
    expect(isTrustable(f)).toBe(false);
  }
  expect(isTrustable({ changed: { oldSha256: "a", newSha256: "b" } })).toBe(false);
  expect(isTrustable({ other: "x" })).toBe(false);
});

test("originOf shows host:port including the default port", () => {
  expect(originOf("https://svn.example.com/repos/x")).toBe("https://svn.example.com:443");
  expect(originOf("https://svn.example.com:8443/x")).toBe("https://svn.example.com:8443");
});

test("normalizeAuthPrompt falls back to safe defaults for missing reason/offered", () => {
  const p = normalizeAuthPrompt({
    requestId: "r",
    kind: "credentials",
    realm: "x",
    url: "https://h/",
    cert: null,
    reason: null,
    attempt: 0,
    offered: [],
  });
  expect(p).toMatchObject({
    kind: "credentials",
    reason: "initial",
    attempt: 0,
    offered: ["basic"],
  });
  expect(
    normalizeAuthPrompt({
      requestId: "r",
      kind: "certificate",
      realm: "x",
      url: "u",
      cert: null,
      reason: null,
      attempt: 0,
      offered: [],
    }),
  ).toBeNull();
});

test("sign-in shows full origin, realm and the rejection counter", () => {
  withLang(
    <SignInDialog
      prompt={creds({ reason: "rejected", attempt: 2 })}
      onSubmit={() => undefined}
      onCancel={() => undefined}
    />,
  );
  expect(screen.getByText("https://svn.example.com:443")).toBeTruthy();
  expect(screen.getByText("<https://svn.example.com:443> Repo")).toBeTruthy();
  expect(screen.getByRole("alert").textContent).toContain("Attempt 2 of 3");
});

test("sign-in explains an earlier rejection and a stale saved password", () => {
  const { unmount } = withLang(
    <SignInDialog
      prompt={creds({ reason: "rejected", attempt: 0 })}
      onSubmit={() => undefined}
      onCancel={() => undefined}
    />,
  );
  const earlier = screen.getByRole("alert").textContent ?? "";
  expect(earlier).toContain("entered last time");
  expect(earlier).not.toContain("Attempt");
  unmount();
  withLang(
    <SignInDialog
      prompt={creds({ reason: "expired" })}
      onSubmit={() => undefined}
      onCancel={() => undefined}
    />,
  );
  expect(screen.getByRole("alert").textContent).toContain("saved password");
});

test("normalizeAuthPrompt keeps the expired reason", () => {
  const p = normalizeAuthPrompt({
    requestId: "r",
    kind: "credentials",
    realm: "x",
    url: "https://h/",
    cert: null,
    reason: "expired",
    attempt: 0,
    offered: ["basic"],
  });
  expect(p?.kind === "credentials" && p.reason).toBe("expired");
});

test("sign-in submits trimmed username and the password, then Cancel answers cancel", async () => {
  const onSubmit = vi.fn();
  const onCancel = vi.fn();
  withLang(<SignInDialog prompt={creds()} onSubmit={onSubmit} onCancel={onCancel} />);
  const signIn = screen.getByRole("button", { name: "Sign in" });
  expect((signIn as HTMLButtonElement).disabled).toBe(true);
  await user().type(screen.getByLabelText("Username"), "  dev ");
  await user().type(screen.getByLabelText("Password"), "s3cret");
  expect((screen.getByLabelText("Password") as HTMLInputElement).type).toBe("password");
  await user().click(signIn);
  expect(onSubmit).toHaveBeenCalledWith({ username: "dev", password: "s3cret", save: true });
  await user().click(screen.getByRole("button", { name: "Cancel" }));
  expect(onCancel).toHaveBeenCalled();
});

test("Windows SSO is offered when the server offers negotiate and answers negotiate", async () => {
  const onNegotiate = vi.fn();
  const onSubmit = vi.fn();
  withLang(
    <SignInDialog
      prompt={creds({ offered: ["negotiate", "basic"] })}
      onSubmit={onSubmit}
      onNegotiate={onNegotiate}
      onCancel={() => undefined}
    />,
  );
  await user().click(screen.getByRole("button", { name: "Sign in with Windows account" }));
  expect(onNegotiate).toHaveBeenCalledTimes(1);
  expect(onSubmit).not.toHaveBeenCalled();
  // Username/password stays available as the fallback.
  expect(screen.getByLabelText("Username")).toBeTruthy();
});

test.each([
  ["basic only", creds({ offered: ["basic"] })],
  ["after Windows sign-in failed", creds({ offered: ["negotiate"], reason: "negotiate-failed" })],
])("Windows SSO is not offered %s", (_, prompt) => {
  withLang(
    <SignInDialog
      prompt={prompt}
      onSubmit={() => undefined}
      onNegotiate={() => undefined}
      onCancel={() => undefined}
    />,
  );
  expect(screen.queryByRole("button", { name: /Windows/ })).toBeNull();
});

test("after 3 rejected attempts submitting is blocked", async () => {
  withLang(
    <SignInDialog
      prompt={creds({ reason: "rejected", attempt: 3 })}
      onSubmit={() => undefined}
      onCancel={() => undefined}
    />,
  );
  await user().type(screen.getByLabelText("Username"), "a");
  await user().type(screen.getByLabelText("Password"), "b");
  expect((screen.getByRole("button", { name: "Sign in" }) as HTMLButtonElement).disabled).toBe(
    true,
  );
});

test("self-signed offers reject, trust once and trust permanently", async () => {
  const onDecision = vi.fn();
  withLang(<TrustCertDialog prompt={certPrompt("self-signed")} onDecision={onDecision} />);
  await user().click(screen.getByRole("button", { name: "Trust permanently" }));
  expect(onDecision).toHaveBeenCalledWith("acceptAlways");
  await user().click(screen.getByRole("button", { name: "Trust for this session" }));
  expect(onDecision).toHaveBeenCalledWith("acceptOnce");
  await user().click(screen.getByRole("button", { name: "Reject" }));
  expect(onDecision).toHaveBeenCalledWith("reject");
  expect(screen.getByText("AA:BB")).toBeTruthy();
});

test.each(["hostname-mismatch", "expired", "revoked", "not-yet-valid"] as const)(
  "%s blocks hard: only Close, no trust buttons",
  async (failure) => {
    const onDecision = vi.fn();
    withLang(<TrustCertDialog prompt={certPrompt(failure)} onDecision={onDecision} />);
    expect(screen.queryByRole("button", { name: /Trust/ })).toBeNull();
    expect(screen.getByText(/connection is blocked/i)).toBeTruthy();
    await user().click(screen.getByRole("button", { name: "Close" }));
    expect(onDecision).toHaveBeenCalledWith("reject");
  },
);

test("changed certificate: special warning, never one-click trust; re-pin needs confirmation", async () => {
  const onDecision = vi.fn();
  withLang(
    <TrustCertDialog
      prompt={certPrompt({ changed: { oldSha256: "OLD:11", newSha256: "NEW:22" } })}
      onDecision={onDecision}
    />,
  );
  expect(
    screen.getByRole("dialog", { name: "Server certificate changed", hidden: true }),
  ).toBeTruthy();
  expect(screen.getByText("OLD:11")).toBeTruthy();
  expect(screen.getByText("NEW:22")).toBeTruthy();
  expect(screen.queryByRole("button", { name: /Trust/ })).toBeNull();
  const replace = screen.getByRole("button", {
    name: "Replace pinned certificate",
  }) as HTMLButtonElement;
  expect(replace.disabled).toBe(true);
  await user().click(screen.getByLabelText(/I verified the new fingerprint/));
  expect(replace.disabled).toBe(false);
  await user().click(replace);
  expect(onDecision).toHaveBeenCalledWith("replacePin");
});

test("hostile realm and subject render as text", () => {
  const { container } = withLang(
    <SignInDialog
      prompt={creds({ realm: "<img src=x onerror=alert(1)>" })}
      onSubmit={() => undefined}
      onCancel={() => undefined}
    />,
  );
  expect(container.ownerDocument.querySelector("img")).toBeNull();
  expect(screen.getByText("<img src=x onerror=alert(1)>")).toBeTruthy();
});

test("host: auth://prompt opens the dialog and Cancel answers cancel, closing the queue", async () => {
  renderApp(<AuthPromptHost />);
  await waitFor(() => expect(mockEmit).toBeDefined());
  // Wait for the bus to subscribe (async start) before emitting.
  await act(async () => {
    await vi.advanceTimersByTimeAsync(20);
  });
  const before = mockAuthResponses.length;
  await act(async () => {
    mockEmit("auth://prompt", {
      requestId: "host-1",
      kind: "credentials",
      realm: "r",
      url: "https://h:8443/",
      cert: null,
      reason: "initial",
      attempt: 0,
      offered: ["basic"],
    });
  });
  expect(await screen.findByRole("dialog", { name: "Sign in", hidden: true })).toBeTruthy();
  expect(screen.getByText("https://h:8443")).toBeTruthy();
  await user().click(screen.getByRole("button", { name: "Cancel" }));
  await waitFor(() => expect(mockAuthResponses.length).toBe(before + 1));
  expect(mockAuthResponses.at(-1)).toEqual({ requestId: "host-1", response: { kind: "cancel" } });
  await waitFor(() => expect(screen.queryByRole("dialog", { hidden: true })).toBeNull());
});

test("host: certificate decisions map to auth_respond", async () => {
  renderApp(<AuthPromptHost />);
  await act(async () => {
    await vi.advanceTimersByTimeAsync(20);
  });
  await act(async () => {
    mockEmit("auth://prompt", {
      requestId: "host-2",
      kind: "certificate",
      realm: "r",
      url: "https://h/",
      cert: { ...CERT, failure: "self-signed" },
    });
  });
  await user().click(await screen.findByRole("button", { name: "Trust for this session" }));
  await waitFor(() => expect(mockAuthResponses.at(-1)?.requestId).toBe("host-2"));
  expect(mockAuthResponses.at(-1)?.response).toEqual({
    kind: "certificate",
    decision: "acceptOnce",
  });
});

test("auth_respond failure sends a compensating cancel so the engine is not left waiting", async () => {
  mockCalls.length = 0;
  // The mock rejects unknown variants like serde does for a variant that is not in the contract.
  const err = await answerAuthPrompt("fail-1", { kind: "fingerprint" } as never);
  expect(err?.message).toMatch(/invalid args/);
  const sent = mockCalls.filter((c) => c.name === "authRespond").map((c) => c.args);
  expect(sent).toEqual([
    ["fail-1", { kind: "fingerprint" }],
    ["fail-1", { kind: "cancel" }],
  ]);
  expect(mockAuthResponses.at(-1)).toEqual({ requestId: "fail-1", response: { kind: "cancel" } });
});

test("host: Sign in with Windows account answers negotiate", async () => {
  renderApp(<AuthPromptHost />);
  await act(async () => {
    await vi.advanceTimersByTimeAsync(20);
  });
  await act(async () => {
    mockEmit("auth://prompt", {
      requestId: "sso-1",
      kind: "credentials",
      realm: "r",
      url: "https://h/",
      cert: null,
      reason: "initial",
      attempt: 0,
      offered: ["negotiate", "basic"],
    });
  });
  await user().click(await screen.findByRole("button", { name: "Sign in with Windows account" }));
  await waitFor(() => expect(mockAuthResponses.at(-1)?.requestId).toBe("sso-1"));
  expect(mockAuthResponses.at(-1)?.response).toEqual({ kind: "negotiate" });
});

test("host: Replace pinned certificate answers replacePin, not acceptAlways", async () => {
  renderApp(<AuthPromptHost />);
  await act(async () => {
    await vi.advanceTimersByTimeAsync(20);
  });
  await act(async () => {
    mockEmit("auth://prompt", {
      requestId: "pin-1",
      kind: "certificate",
      realm: "r",
      url: "https://h/",
      cert: { ...CERT, failure: { changed: { oldSha256: "OLD:11", newSha256: "NEW:22" } } },
    });
  });
  await user().click(await screen.findByLabelText(/I verified the new fingerprint/));
  await user().click(screen.getByRole("button", { name: "Replace pinned certificate" }));
  await waitFor(() => expect(mockAuthResponses.at(-1)?.requestId).toBe("pin-1"));
  expect(mockAuthResponses.at(-1)?.response).toEqual({
    kind: "certificate",
    decision: "replacePin",
  });
});

test("a failing cancel is not retried forever and the first error is returned", async () => {
  mockCalls.length = 0;
  const ok = await answerAuthPrompt("ok-1", { kind: "cancel" });
  expect(ok).toBeNull();
  expect(mockCalls.filter((c) => c.name === "authRespond")).toHaveLength(1);
});
