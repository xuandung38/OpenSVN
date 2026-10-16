import { useId, useState } from "react";
import { Button } from "../../../components/ui/button";
import { Dialog } from "../../../components/ui/dialog";
import { Input } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { type CredentialsPromptInfo, MAX_AUTH_ATTEMPTS, originOf } from "./auth-model";
import { authStrings } from "./auth-strings";

export interface CredentialsAnswer {
  username: string;
  password: string;
  save: boolean;
}

/**
 * Sign-in dialog (doc 01, 4.3). Shows the full origin (host:port), the realm and a reason-specific
 * message. Esc/Close cancels the operation.
 *
 * When the server offers `negotiate`, "Sign in with Windows account" answers `{kind:"negotiate"}`
 * (single sign-on, no secret). It is not offered again after Windows sign-in failed for this server.
 */
export function SignInDialog({
  prompt,
  onSubmit,
  onNegotiate,
  onCancel,
}: {
  prompt: CredentialsPromptInfo;
  onSubmit: (a: CredentialsAnswer) => void;
  /** Single sign-on with the Windows account; shown only when the server offered `negotiate`. */
  onNegotiate?: () => void;
  onCancel: () => void;
}) {
  const t = useT(authStrings);
  const c = useT(commonStrings);
  const formId = useId();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [save, setSave] = useState(true);
  const exhausted = prompt.reason === "rejected" && prompt.attempt >= MAX_AUTH_ATTEMPTS;
  const sso =
    onNegotiate !== undefined &&
    prompt.offered.includes("negotiate") &&
    prompt.reason !== "negotiate-failed";

  // `rejected` at attempt 0: the password typed last time was rejected (and so not saved), but
  // this is a new sign-in round, so there is no attempt counter to show.
  const reason =
    prompt.reason === "rejected" && prompt.attempt === 0
      ? t("reasonRejectedEarlier")
      : prompt.reason === "rejected"
        ? t("reasonRejected", {
            attempt: Math.min(prompt.attempt, MAX_AUTH_ATTEMPTS),
            max: MAX_AUTH_ATTEMPTS,
          })
        : prompt.reason === "negotiate-failed"
          ? t("reasonNegotiateFailed")
          : prompt.reason === "expired"
            ? t("reasonExpired")
            : t("reasonInitial");

  const valid = username.trim().length > 0 && password.length > 0;
  const submit = () => {
    if (valid && !exhausted) onSubmit({ username: username.trim(), password, save });
  };

  return (
    <Dialog
      open
      onClose={onCancel}
      title={t("signInTitle")}
      footer={
        <>
          <Button onClick={onCancel}>{c("cancel")}</Button>
          <Button variant="primary" type="submit" form={formId} disabled={!valid || exhausted}>
            {t("signInAction")}
          </Button>
        </>
      }
    >
      <dl className="m-0 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
        <dt className="text-muted-text">{t("server")}</dt>
        <dd className="m-0 font-mono text-[12px]">
          <SafeText>{originOf(prompt.url)}</SafeText>
        </dd>
        <dt className="text-muted-text">{t("realm")}</dt>
        <dd className="m-0 break-all">
          <SafeText>{prompt.realm}</SafeText>
        </dd>
      </dl>
      <p
        className={prompt.reason === "initial" ? "m-0" : "m-0 rounded-md bg-warn-soft px-2.5 py-2"}
        role={prompt.reason === "initial" ? undefined : "alert"}
      >
        {reason}
      </p>
      {exhausted && <p className="m-0 text-bad-text">{t("lastAttempt")}</p>}
      {sso && (
        <div className="flex flex-col gap-2">
          <Button onClick={onNegotiate}>{t("signInWindows")}</Button>
          <p className="m-0 text-[12px] text-muted-text">{t("orPassword")}</p>
        </div>
      )}
      <form
        id={formId}
        className="flex flex-col gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <label className="flex flex-col gap-1">
          {t("username")}
          <Input
            autoComplete="username"
            autoCapitalize="off"
            spellCheck={false}
            value={username}
            onChange={(e) => setUsername(e.target.value)}
          />
        </label>
        <label className="flex flex-col gap-1">
          {t("password")}
          <Input
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </label>
        <label className="flex items-center gap-2">
          <input
            type="checkbox"
            checked={save}
            onChange={(e) => setSave(e.target.checked)}
            className="accent-[var(--accent)]"
          />
          {t("remember")}
        </label>
      </form>
    </Dialog>
  );
}
