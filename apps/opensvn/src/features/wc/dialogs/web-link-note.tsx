import { SafeText } from "../../../components/ui/safe-text";
import { useT } from "../../../lib/i18n/i18n";
import { hasQueryOrFragment, isVisualSvnWebLink, type WebLinkState } from "../../remote/web-link";
import { dialogStrings } from "./dialog-strings";

/**
 * Why a repository URL field holds something other than a plain repository URL: a VisualSVN
 * web link being checked, converted or refused, or any other URL with `?` or `#`.
 * Renders nothing when there is nothing to say; `fallback` is shown for any other invalid URL.
 */
export function UrlFieldNote({
  url,
  state,
  revFilled,
  fallback,
}: {
  url: string;
  state: WebLinkState;
  /** Revision taken from the link and put in the revision field, if any. */
  revFilled: number | null;
  fallback: string | null;
}) {
  const d = useT(dialogStrings);
  switch (state.kind) {
    case "checking":
      return (
        <span className="text-muted-text" aria-live="polite">
          {d("webLinkChecking")}
        </span>
      );
    case "converted":
      return (
        <>
          <SafeText as="p" className="m-0 break-words text-[12px] text-muted-text">
            <span role="status">
              {d("webLinkConverted", { url: state.to })}
              {revFilled !== null && ` ${d("webLinkRev", { rev: revFilled })}`}
            </span>
          </SafeText>
          {fallback !== null && (
            <span className="text-bad-text" role="alert">
              {fallback}
            </span>
          )}
        </>
      );
    case "failed":
      return (
        <div className="flex flex-col gap-0.5">
          <span className="text-bad-text" role="alert">
            {d("webLinkFailed")}
          </span>
          <SafeText as="span" className="break-words text-[12px] text-muted-text">
            {state.detail}
          </SafeText>
        </div>
      );
    case "idle":
      if (url.trim().length === 0 || isVisualSvnWebLink(url)) return null;
      if (hasQueryOrFragment(url)) {
        return (
          <span className="text-bad-text" role="alert">
            {d("urlQueryFragment")}
          </span>
        );
      }
      return fallback === null ? null : (
        <span className="text-bad-text" role="alert">
          {fallback}
        </span>
      );
  }
}
