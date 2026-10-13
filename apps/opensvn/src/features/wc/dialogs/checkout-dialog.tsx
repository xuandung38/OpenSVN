import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { Input, Select } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { Depth, PathGrantInfo } from "../../../ipc/bindings";
import { IpcError, ipc, unwrap } from "../../../ipc/client";
import { useT } from "../../../lib/i18n/i18n";
import { parseRevision } from "../../remote/branch/branch-model";
import { isCheckoutUrl } from "../wc-model";
import { pickFolder } from "../welcome/use-path-grant";
import { ActionDialog } from "./action-dialog";
import { dialogStrings } from "./dialog-strings";

const DEPTHS: {
  value: Depth;
  label: "depthInfinity" | "depthImmediates" | "depthFiles" | "depthEmpty";
}[] = [
  { value: "infinity", label: "depthInfinity" },
  { value: "immediates", label: "depthImmediates" },
  { value: "files", label: "depthFiles" },
  { value: "empty", label: "depthEmpty" },
];

/**
 * Checkout (doc 01 4.1). The destination is chosen in the native folder dialog, which yields a
 * single-use grant; the app never handles the raw path. An expired or consumed grant is dropped so
 * the user picks the folder again.
 */
export function CheckoutDialog({ onClose }: { onClose: () => void }) {
  const d = useT(dialogStrings);
  const toast = useToast();
  const [url, setUrl] = useState("");
  const [revText, setRevText] = useState("");
  const [depth, setDepth] = useState<Depth>("infinity");
  const [ignoreExternals, setIgnoreExternals] = useState(false);
  const [dest, setDest] = useState<PathGrantInfo | null>(null);
  const [pickError, setPickError] = useState<string | null>(null);

  const rev = parseRevision(revText);
  const urlOk = isCheckoutUrl(url);
  const canSubmit = urlOk && dest !== null && rev !== "invalid";

  const choose = async () => {
    setPickError(null);
    try {
      const g = await pickFolder(d("destination"));
      if (g) setDest(g);
    } catch (e) {
      setPickError(e instanceof IpcError ? e.message : String(e));
    }
  };

  return (
    <ActionDialog
      title={d("checkoutTitle")}
      submitLabel={d("checkoutAction")}
      canSubmit={canSubmit}
      onClose={onClose}
      onSubmit={async () => {
        if (!dest || rev === "invalid") return;
        try {
          await unwrap(
            ipc().commands.checkout({
              url: url.trim(),
              dest: dest.id,
              rev,
              depth,
              sparse: null,
              format: 31,
              storePristine: true,
              ignoreExternals,
            }),
          );
        } catch (e) {
          // Grants are single-use: whatever failed, the folder must be picked again.
          setDest(null);
          if (e instanceof IpcError && e.appError.code === "invalid-argument") {
            throw new Error(d("grantExpired"));
          }
          throw e;
        }
        toast.show(d("checkoutStarted"));
        onClose();
      }}
    >
      <label className="flex flex-col gap-1">
        {d("repoUrl")}
        <Input
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          placeholder={d("repoUrlHint")}
          aria-invalid={url.length > 0 && !urlOk}
          spellCheck={false}
        />
      </label>
      {url.length > 0 && !urlOk && (
        <span className="text-bad-text" role="alert">
          {d("urlInvalid")}
        </span>
      )}
      <div className="flex flex-col gap-1">
        <span id="wc-checkout-dest">{d("destination")}</span>
        <div className="flex items-center gap-2">
          <SafeText
            as="span"
            className="min-w-0 flex-1 truncate rounded-md border border-line bg-surface-2 px-2 py-1.5 font-mono text-[12px]"
          >
            {dest ? dest.displayName : d("noFolder")}
          </SafeText>
          <Button aria-describedby="wc-checkout-dest" onClick={() => void choose()}>
            {d("chooseFolder")}
          </Button>
        </div>
        {pickError && (
          <span className="text-bad-text" role="alert">
            {pickError}
          </span>
        )}
      </div>
      <div className="grid grid-cols-2 gap-2.5">
        <label className="flex flex-col gap-1">
          {d("revision")}
          <Input
            value={revText}
            onChange={(e) => setRevText(e.target.value)}
            placeholder={d("revisionHint")}
            aria-invalid={rev === "invalid"}
          />
        </label>
        <label className="flex flex-col gap-1">
          {d("depth")}
          <Select value={depth} onChange={(e) => setDepth(e.target.value as Depth)}>
            {DEPTHS.map((o) => (
              <option key={o.value} value={o.value}>
                {d(o.label)}
              </option>
            ))}
          </Select>
        </label>
      </div>
      {rev === "invalid" && (
        <span className="text-bad-text" role="alert">
          {d("revisionInvalid")}
        </span>
      )}
      <label className="flex items-center gap-2">
        <input
          type="checkbox"
          checked={ignoreExternals}
          onChange={(e) => setIgnoreExternals(e.target.checked)}
          className="accent-[var(--accent)]"
        />
        {d("ignoreExternals")}
      </label>
      <span className="text-[12px] text-muted-text">{d("formatNote")}</span>
    </ActionDialog>
  );
}
