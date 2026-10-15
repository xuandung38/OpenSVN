import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { BadBox, Dialog, WarnBox } from "../../../components/ui/dialog";
import { SafeText } from "../../../components/ui/safe-text";
import type { CertDecision } from "../../../ipc/bindings";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { type CertPromptInfo, isChanged, isTrustable, originOf } from "./auth-model";
import { authStrings } from "./auth-strings";

type Key = keyof (typeof authStrings)["en"];

function failureKey(f: CertPromptInfo["failure"]): Key {
  if (typeof f === "object") return isChanged(f) ? "failureChanged" : "failureOther";
  switch (f) {
    case "unknown-issuer":
      return "failureUnknownIssuer";
    case "self-signed":
      return "failureSelfSigned";
    case "expired":
      return "failureExpired";
    case "not-yet-valid":
      return "failureNotYetValid";
    case "hostname-mismatch":
      return "failureHostname";
    case "revoked":
      return "failureRevoked";
  }
}

/**
 * Trust-certificate dialog (doc 01, 4.4). Permanent trust is offered only for an unknown issuer or
 * a self-signed certificate. Hostname mismatch, expired, not-yet-valid, revoked and unrecognised
 * failures block hard (Close only). A changed pinned certificate gets its own warning and can only
 * be replaced through an explicit, confirmed re-pin; it is never "always trust" by one click.
 */
export function TrustCertDialog({
  prompt,
  onDecision,
}: {
  prompt: CertPromptInfo;
  onDecision: (d: CertDecision) => void;
}) {
  const t = useT(authStrings);
  const c = useT(commonStrings);
  const [confirmedPin, setConfirmedPin] = useState(false);
  const f = prompt.failure;
  const changed = isChanged(f);
  const trustable = isTrustable(f);
  const detail = typeof f === "object" && "other" in f ? f.other : "";
  const reject = () => onDecision("reject");

  return (
    <Dialog
      open
      onClose={reject}
      title={changed ? t("certChangedTitle") : t("certTitle")}
      footer={
        <>
          <Button onClick={reject}>{trustable || changed ? t("reject") : c("close")}</Button>
          {trustable && (
            <>
              <Button onClick={() => onDecision("acceptOnce")}>{t("trustOnce")}</Button>
              <Button variant="primary" onClick={() => onDecision("acceptAlways")}>
                {t("trustAlways")}
              </Button>
            </>
          )}
          {changed && (
            <Button
              variant="danger"
              disabled={!confirmedPin}
              onClick={() => onDecision("replacePin")}
            >
              {t("replacePin")}
            </Button>
          )}
        </>
      }
    >
      {changed ? (
        <BadBox>
          <b>{t("failureChanged")}</b>
        </BadBox>
      ) : trustable ? (
        <WarnBox>{t(failureKey(f), { detail })}</WarnBox>
      ) : (
        <>
          <BadBox>{t(failureKey(f), { detail })}</BadBox>
          <p className="m-0">{t("blocked")}</p>
        </>
      )}
      <dl className="m-0 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
        <dt className="text-muted-text">{t("host")}</dt>
        <dd className="m-0 font-mono text-[12px]">
          <SafeText>{originOf(prompt.url)}</SafeText>
        </dd>
        <dt className="text-muted-text">{t("subject")}</dt>
        <dd className="m-0 break-all">
          <SafeText>{prompt.cert.subject}</SafeText>
        </dd>
        <dt className="text-muted-text">{t("issuer")}</dt>
        <dd className="m-0 break-all">
          <SafeText>{prompt.cert.issuer}</SafeText>
        </dd>
        <dt className="text-muted-text">{t("validity")}</dt>
        <dd className="m-0 tnum">
          {prompt.cert.notBefore} → {prompt.cert.notAfter}
        </dd>
        {changed ? (
          <>
            <dt className="text-muted-text">{t("oldFingerprint")}</dt>
            <dd className="m-0 break-all font-mono text-[11px]">{f.changed.oldSha256}</dd>
            <dt className="text-muted-text">{t("newFingerprint")}</dt>
            <dd className="m-0 break-all font-mono text-[11px]">{f.changed.newSha256}</dd>
          </>
        ) : (
          <>
            <dt className="text-muted-text">{t("fingerprint")}</dt>
            <dd className="m-0 break-all font-mono text-[11px]">{prompt.cert.sha256}</dd>
          </>
        )}
      </dl>
      {changed && (
        <label className="flex items-start gap-2">
          <input
            type="checkbox"
            checked={confirmedPin}
            onChange={(e) => setConfirmedPin(e.target.checked)}
            className="mt-0.5 accent-[var(--accent)]"
          />
          {t("replacePinConfirm")}
        </label>
      )}
    </Dialog>
  );
}
