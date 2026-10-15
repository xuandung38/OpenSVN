import { Banner } from "../../../components/ui/banner";
import { Button } from "../../../components/ui/button";
import type { ResumeInfo } from "../../../ipc/bindings";
import { useT } from "../../../lib/i18n/i18n";
import { resumeStrings } from "./resume-strings";
import { useResumeActions } from "./use-resume-actions";

/**
 * Banner of an incomplete working copy (an interrupted checkout or update): "Continue" at the
 * revision it was reaching, or "Update to latest instead". Not an error and not a cleanup case.
 */
export function IncompleteBanner({ wcId, resume }: { wcId: string; resume: ResumeInfo }) {
  const t = useT(resumeStrings);
  const actions = useResumeActions();
  return (
    <Banner
      title={t(resume.checkout ? "checkoutIncompleteTitle" : "updateIncompleteTitle")}
      actions={
        <>
          <Button small variant="primary" onClick={() => void actions.continueWc(wcId)}>
            {t(resume.checkout ? "continueCheckout" : "continueUpdate")}
          </Button>
          <Button small onClick={() => void actions.updateLatest(wcId)}>
            {t("updateLatest")}
          </Button>
        </>
      }
    >
      {t(resume.checkout ? "checkoutIncompleteText" : "updateIncompleteText", { rev: resume.rev })}
    </Banner>
  );
}
