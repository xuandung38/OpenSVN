import { useState } from "react";
import { Banner } from "../../components/ui/banner";
import { Button } from "../../components/ui/button";
import type { Banner as BannerData } from "../../ipc/bindings";
import { useBanners } from "../../ipc/queries";
import { useT } from "../../lib/i18n/i18n";
import { useWcActions } from "./use-wc-actions";
import { isConflicted } from "./wc-model";
import { useWcSession } from "./wc-session";
import { wcStrings } from "./wc-strings";

/**
 * Banner strip (doc 01 3.6): conflicts (derived from the status, always current) plus the
 * backend-pushed `wc://banner` kinds. Pushed banners can be dismissed for the session.
 */
export function WcBanners() {
  const { wc, files, openDialog } = useWcSession();
  const t = useT(wcStrings);
  const actions = useWcActions();
  const pushed = useBanners(wc?.id ?? null).data;
  const [dismissed, setDismissed] = useState<ReadonlySet<string>>(new Set());
  const conflicts = files.filter(isConflicted).length;

  const dismiss = (b: BannerData) => (
    <Button small onClick={() => setDismissed(new Set([...dismissed, `${b.wcId}:${b.kind}`]))}>
      {t("bannerDismiss")}
    </Button>
  );
  const pending = (pushed ?? []).filter(
    (b) => b.kind !== "conflicts" && !dismissed.has(`${b.wcId}:${b.kind}`),
  );

  return (
    <>
      {conflicts > 0 && (
        <Banner
          title={t("bannerConflicts", { count: conflicts })}
          actions={
            <Button small onClick={() => openDialog({ kind: "resolver", path: null })}>
              {t("bannerResolve")}
            </Button>
          }
        >
          {t("bannerConflictsText")}
        </Banner>
      )}
      {pending.map((b) => {
        switch (b.kind) {
          case "mixed-rev":
            return (
              <Banner
                key={b.kind}
                title={t("bannerMixedRev")}
                actions={
                  <>
                    <Button small onClick={() => void actions.update(null)}>
                      {t("bannerUpdateAll")}
                    </Button>
                    {dismiss(b)}
                  </>
                }
              >
                {t("bannerMixedRevText")}
              </Banner>
            );
          case "lock-stolen":
            return (
              <Banner key={b.kind} tone="danger" title={t("bannerLockStolen")} actions={dismiss(b)}>
                {t("bannerLockStolenText", { paths: b.data.join(", ") })}
              </Banner>
            );
          case "needs-cleanup":
            return (
              <Banner
                key={b.kind}
                tone="danger"
                title={t("bannerNeedsCleanup")}
                actions={
                  <Button small onClick={() => openDialog({ kind: "cleanup" })}>
                    {t("bannerCleanup")}
                  </Button>
                }
              >
                {t("bannerNeedsCleanupText")}
              </Banner>
            );
          default:
            return null;
        }
      })}
    </>
  );
}
