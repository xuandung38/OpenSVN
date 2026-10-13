import { Button } from "../components/ui/button";
import { mockEmit } from "../ipc/mock/adapter";

const CERT = {
  host: "svn.example.com",
  subject: "CN=svn.example.com, O=Example Corp",
  issuer: "CN=Example Internal CA",
  notBefore: "2026-01-01",
  notAfter: "2027-01-01",
  sha256:
    "AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89",
};

let seq = 0;
const cred = (extra: Record<string, unknown>) =>
  mockEmit("auth://prompt", {
    requestId: `demo-${++seq}`,
    kind: "credentials",
    realm: "<https://svn.example.com:443> Game repository",
    url: "https://svn.example.com/repos/game",
    cert: null,
    ...extra,
  });
const cert = (failure: unknown) =>
  mockEmit("auth://prompt", {
    requestId: `demo-${++seq}`,
    kind: "certificate",
    realm: "svn.example.com:443",
    url: "https://svn.example.com/repos/game",
    cert: { ...CERT, failure },
  });

/** Dev-only buttons that fire mock `auth://prompt` events so the dialogs can be reviewed. */
export function RemoteDemo() {
  return (
    <section aria-label="Remote dialogs" className="flex flex-wrap gap-2 border-b border-line p-4">
      <b className="w-full text-[10px] font-medium tracking-[.07em] text-muted-text uppercase">
        Remote dialogs (mock events)
      </b>
      <Button onClick={() => cred({ reason: "initial", attempt: 0, offered: ["basic"] })}>
        Sign in
      </Button>
      <Button onClick={() => cred({ reason: "rejected", attempt: 2, offered: ["basic"] })}>
        Sign in (rejected 2)
      </Button>
      <Button
        onClick={() =>
          cred({ reason: "negotiate-failed", attempt: 1, offered: ["negotiate", "basic"] })
        }
      >
        Sign in (SSO failed)
      </Button>
      <Button onClick={() => cert("self-signed")}>Cert: self-signed</Button>
      <Button onClick={() => cert("unknown-issuer")}>Cert: unknown issuer</Button>
      <Button onClick={() => cert("hostname-mismatch")}>Cert: hostname mismatch</Button>
      <Button onClick={() => cert("expired")}>Cert: expired</Button>
      <Button
        onClick={() => cert({ changed: { oldSha256: "11:22:33:44", newSha256: CERT.sha256 } })}
      >
        Cert: changed
      </Button>
    </section>
  );
}
