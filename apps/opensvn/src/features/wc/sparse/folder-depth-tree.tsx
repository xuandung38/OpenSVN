import { ChevronDown, ChevronRight, Folder } from "lucide-react";
import { useState } from "react";
import { Select } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import type { Depth, SparseDepth } from "../../../ipc/bindings";
import { toAppError } from "../../../ipc/client";
import { cn } from "../../../lib/cn";
import { useT, type Vars } from "../../../lib/i18n/i18n";
import { useRepoList } from "../../remote/browse/use-repo";
import { joinUrl } from "../../remote/repo-url";
import { type Choices, type Effective, effective, joinPath } from "./sparse-model";
import { sparseStrings } from "./sparse-strings";

type T = (key: keyof (typeof sparseStrings)["en"], vars?: Vars) => string;

const DEPTH_KEYS = {
  infinity: "depthInfinity",
  immediates: "depthImmediates",
  files: "depthFiles",
  empty: "depthEmpty",
  exclude: "depthExclude",
} as const;

const OPTIONS: SparseDepth[] = ["infinity", "immediates", "files", "empty", "exclude"];

function effLabel(e: Effective, t: T): string {
  return e === "out" ? t("out") : t(DEPTH_KEYS[e]);
}

export interface FolderDepthTreeProps {
  /** URL of the checkout root (the working copy's URL for an existing one). */
  rootUrl: string;
  /** Revision to list (`null` = HEAD). */
  rev: number | null;
  rootDepth: Depth;
  choices: Choices;
  /** Sets (or with `null`, clears) the choice of a folder. */
  onChoose: (path: string, depth: SparseDepth | null) => void;
  className?: string;
}

/**
 * The folders below a checkout root, listed from the repository as they are expanded, each with
 * a depth: its own choice, or "as above" (what the folder above gives it). Names are shown as text
 * only. Native controls (buttons and selects in nested lists) keep it usable with the keyboard and
 * a screen reader without a custom tree widget.
 */
export function FolderDepthTree({ className, ...props }: FolderDepthTreeProps) {
  const t = useT(sparseStrings);
  return (
    <section
      aria-label={t("treeLabel")}
      className={cn("overflow-auto rounded-md border border-line p-1.5", className)}
    >
      <p className="m-0 mb-1 px-1 text-[12px] text-muted-text">{t("treeHint")}</p>
      <FolderChildren {...props} path="" url={props.rootUrl} level={0} />
    </section>
  );
}

function FolderChildren({
  path,
  url,
  level,
  ...props
}: Omit<FolderDepthTreeProps, "className"> & { path: string; url: string; level: number }) {
  const t = useT(sparseStrings);
  const list = useRepoList(url, props.rev);
  if (list.isPending) {
    return <p className="m-0 px-1 py-0.5 text-[12px] text-muted-text">{t("loading")}</p>;
  }
  if (list.error) {
    return (
      <SafeText as="p" className="m-0 px-1 py-0.5 text-[12px] text-bad-text">
        {t("loadError", { message: toAppError(list.error).message })}
      </SafeText>
    );
  }
  const dirs = list.data.filter((e) => e.kind === "dir");
  if (dirs.length === 0) {
    return <p className="m-0 px-1 py-0.5 text-[12px] text-muted-text">{t("noFolders")}</p>;
  }
  return (
    <ul className="m-0 list-none p-0">
      {dirs.map((d) => (
        <FolderRow
          key={d.name}
          {...props}
          name={d.name}
          path={joinPath(path, d.name)}
          url={joinUrl(url, d.name)}
          level={level}
        />
      ))}
    </ul>
  );
}

function FolderRow({
  name,
  path,
  url,
  level,
  ...props
}: Omit<FolderDepthTreeProps, "className"> & {
  name: string;
  path: string;
  url: string;
  level: number;
}) {
  const t = useT(sparseStrings);
  const [open, setOpen] = useState(false);
  const own = props.choices.get(path);
  const eff = effective(path, props.rootDepth, props.choices);
  const without = new Map(props.choices);
  without.delete(path);
  const inherited = effective(path, props.rootDepth, without);
  return (
    <li>
      <div
        className="flex items-center gap-1.5 py-0.5"
        // Indentation by level; CSSOM style is allowed by the CSP.
        style={{ paddingLeft: `${level * 16}px` }}
      >
        <button
          type="button"
          aria-expanded={open}
          aria-label={open ? t("collapse", { path }) : t("expand", { path })}
          onClick={() => setOpen((o) => !o)}
          className="flex size-5 cursor-pointer items-center justify-center rounded-sm border-0 bg-transparent p-0 text-muted-text hover:bg-surface-2"
        >
          {open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
        </button>
        <Folder size={14} aria-hidden="true" className="shrink-0 text-muted-text" />
        <SafeText className={cn("min-w-0 flex-1 truncate", eff === "out" && "text-muted-text")}>
          {name}
        </SafeText>
        {eff === "out" && <span className="text-[11px] text-muted-text">{t("out")}</span>}
        <Select
          aria-label={t("depthOf", { path })}
          className="w-52 py-0.5 text-[12px]"
          value={own ?? ""}
          onChange={(e) => {
            const v = e.target.value;
            props.onChoose(path, v === "" ? null : (v as SparseDepth));
          }}
        >
          <option value="">{t("follow", { depth: effLabel(inherited, t) })}</option>
          {OPTIONS.map((o) => (
            <option key={o} value={o}>
              {t(DEPTH_KEYS[o])}
            </option>
          ))}
        </Select>
      </div>
      {open && <FolderChildren {...props} path={path} url={url} level={level + 1} />}
    </li>
  );
}
