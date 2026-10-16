import { useState } from "react";
import { Banner } from "../../components/ui/banner";
import { Button } from "../../components/ui/button";
import type { Banner as BannerData, OperationKind, WcId } from "../../ipc/bindings";
import type { OpState } from "../../ipc/events";
import { useBanners, useOperations } from "../../ipc/queries";
import { useT } from "../../lib/i18n/i18n";
import { lockBanner } from "./own-operation";
import { IncompleteBanner } from "./resume/incomplete-banner";
import { useWcActions } from "./use-wc-actions";
import { isConflicted } from "./wc-model";
import { useWcSession } from "./wc-session";
import { wcStrings } from "./wc-strings";

/** Operations whose outcome decides whether the working copy is still out of date. */
const SYNC_KINDS: ReadonlySet<OperationKind> = new Set(["commit", "update", "switch", "checkout"]);

/**
 * Whether the latest commit of `wcId` was refused because the server has newer changes, with no
 * update (or switch) since: the "Out of date" banner, shown in addition to the commit box.
 */
export function outOfDateAfterCommit(ops: readonly OpState[], wcId: WcId): boolean {
  const last = ops
    .filter((o) => o.handle.wcId === wcId && !o.handle.background && SYNC_KINDS.has(o.handle.kind))
    .sort((a, b) => a.startedAt - b.startedAt)
    .at(-1);
  return (
    last?.handle.kind === "commit" &&
    last.state === "failed" &&
    (last.appError?.code === "out-of-date" || last.appError?.actions?.includes("update") === true)
  );
}

/**
 * Banner strip (doc 01 3.6): conflicts (derived from the status, always current), a working copy
 * on a tag, a commit refused as out of date (both also in the commit box), plus the backend-pushed
 * `wc://banner` kinds. Pushed and derived banners can be dismissed for the session.
 */
export function WcBanners() {
  const { wc, files, openDialog } = useWcSession();
  const t = useT(wcStrings);
  const actions = useWcActions();
  const pushed = useBanners(wc?.id ?? null).data;
  const ops = useOperations();
  const [dismissed, setDismissed] = useState<ReadonlySet<string>>(new Set());
  const conflicts = files.filter(isConflicted).length;
  const isDismissed = (kind: string) => wc !== null && dismissed.has(`${wc.id}:${kind}`);
  const dismissDerived = (kind: string) => (
    <Button small onClick={() => wc && setDismissed(new Set([...dismissed, `${wc.id}:${kind}`]))}>
      {t("bannerDismiss")}
    </Button>
  );
  const outOfDate = wc !== null && outOfDateAfterCommit(ops, wc.id) && !isDismissed("out-of-date");
  const tagName = wc?.branch?.kind === "tag" ? wc.branch.name : null;

  const dismiss = (b: BannerData) => (
    <Button small onClick={() => setDismissed(new Set([...dismissed, `${b.wcId}:${b.kind}`]))}>
      {t("bannerDismiss")}
    </Button>
  );
  const pending = (pushed ?? []).filter(
    (b) => b.kind !== "conflicts" && !dismissed.has(`${b.wcId}:${b.kind}`),
  );
  const lock = wc
    ? lockBanner(
        ops,
        wc,
        pending.some((b) => b.kind === "needs-cleanup" || b.kind === "wc-busy"),
      )
    : null;

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
      {outOfDate && (
        <Banner
          title={t("bannerOutOfDate")}
          actions={
            <>
              <Button small onClick={() => void actions.update(null)}>
                {t("bannerUpdate")}
              </Button>
              {dismissDerived("out-of-date")}
            </>
          }
        >
          {t("bannerOutOfDateText")}
        </Banner>
      )}
      {tagName !== null && !isDismissed(`tag:${tagName}`) && (
        <Banner title={t("bannerTag")} actions={dismissDerived(`tag:${tagName}`)}>
          {t("bannerTagText", { name: tagName })}
        </Banner>
      )}
      {wc?.resume && lock?.kind !== "in-progress" && (
        <IncompleteBanner wcId={wc.id} resume={wc.resume} />
      )}
      {lock?.kind === "in-progress" && (
        <Banner title={t("bannerOpInProgress", { title: lock.op.handle.title })}>
          {t("bannerOpInProgressText")}
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
          case "wc-busy":
            // Our own running operation holds the lock: the note above replaces this banner.
            if (lock?.kind === "in-progress") return null;
            return (
              <Banner
                key={b.kind}
                tone="danger"
                title={t("bannerWcBusy")}
                actions={
                  <>
                    <Button
                      small
                      onClick={() => openDialog({ kind: "cleanup", foreignLock: true })}
                    >
                      {t("bannerWcBusyAction")}
                    </Button>
                    {dismiss(b)}
                  </>
                }
              >
                {t("bannerWcBusyText", {
                  dirs:
                    b.data.length === 0 || b.data.includes("")
                      ? t("bannerWcBusyRoot")
                      : b.data.join(", "),
                })}
              </Banner>
            );
          case "needs-cleanup":
            // Our own running operation holds the lock: the note above replaces this banner.
            if (lock?.kind !== "stale") return null;
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
