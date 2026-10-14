import { Banner } from "../components/ui/banner";
import { useEngineCapabilities } from "../ipc/queries";
import { useT } from "../lib/i18n/i18n";
import { shellStrings } from "./shell-strings";

/**
 * Permanent warning while the in-memory demo engine runs in a release build (`demo` capability):
 * nothing the user does reaches a real repository. Not dismissible.
 */
export function DemoEngineBanner() {
  const t = useT(shellStrings);
  const demo = useEngineCapabilities().data?.demo === true;
  if (!demo) return null;
  return (
    <Banner tone="danger" title={t("demoEngine")}>
      {t("demoEngineText")}
    </Banner>
  );
}
