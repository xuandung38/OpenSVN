import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Button } from "../../../components/ui/button";
import { Dialog } from "../../../components/ui/dialog";
import { Input, Textarea } from "../../../components/ui/fields";
import { SafeText } from "../../../components/ui/safe-text";
import { useToast } from "../../../components/ui/toast";
import type { FileKind, ItemProps } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT, type Vars } from "../../../lib/i18n/i18n";
import { useWcSession } from "../wc-session";
import {
  BOOLEAN_PROPS,
  EOL_STYLES,
  knownPropsFor,
  type PropProblem,
  type PropRow,
  propProblem,
  propRows,
} from "./props-model";
import { propsStrings } from "./props-strings";

/** What the editor below the table is doing. */
type Editing = { mode: "add" } | { mode: "edit"; name: string; value: string } | null;

/**
 * Properties of the selected item(s) (doc 01 section 6.1, "Properties…"): view, add, edit and
 * delete through `props_set`, one item after another. Values are checked as svn checks them while
 * typing (`props-model`); the engine re-checks and its refusal is shown inline. With several items a
 * property shows one value when they agree, "(different values)" otherwise, and saving sets it on
 * every item. Binary values are listed but never edited.
 */
export function PropertiesDialog({ paths, onClose }: { paths: string[]; onClose: () => void }) {
  const t = useT(propsStrings);
  const c = useT(commonStrings);
  const toast = useToast();
  const qc = useQueryClient();
  const { wc } = useWcSession();
  const [editing, setEditing] = useState<Editing>(null);
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState(false);

  const key = ["props", wc?.id ?? "", ...paths] as const;
  const query = useQuery<ItemProps[]>({
    queryKey: key,
    enabled: wc !== null,
    retry: false,
    queryFn: () => unwrap(ipc().commands.propsGet(wc?.id ?? "", paths)),
  });
  const items = query.data ?? [];
  const rows = propRows(items);
  const kinds = new Set(items.map((i) => i.kind));
  // A property must suit every item: a file-only name is refused when one of them is a folder.
  const kind: FileKind = kinds.has("dir") ? "dir" : "file";

  const title =
    paths.length === 1
      ? t("propsTitleOne", { path: paths[0] || t("rootName") })
      : t("propsTitleMany", { count: paths.length });

  /** Runs `name = value` (`null` deletes) on every item, then reloads. */
  const apply = async (name: string, value: string | null) => {
    if (!wc) return;
    setPending(true);
    setError(null);
    try {
      for (const p of paths) {
        if (value === null && !items.find((i) => i.path === p)?.props.some((x) => x.name === name))
          continue;
        await unwrap(ipc().commands.propsSet(wc.id, p, name, value));
      }
      toast.show(value === null ? t("propDeleted", { name }) : t("propSaved", { name }));
      setEditing(null);
    } catch (e) {
      setError(toAppError(e).message);
    } finally {
      setPending(false);
      await qc.invalidateQueries({ queryKey: key });
      void qc.invalidateQueries({ queryKey: queryKeys.status(wc.id) });
    }
  };

  return (
    <Dialog
      open
      wide
      onClose={onClose}
      title={title}
      footer={<Button onClick={onClose}>{c("close")}</Button>}
    >
      {query.isPending && <p className="m-0 text-muted-text">{t("propsLoading")}</p>}
      {query.isError && (
        <div className="flex items-center gap-2">
          <SafeText as="p" className="m-0 flex-1 break-words text-bad-text">
            <span role="alert">
              {t("propsLoadFailed")} {toAppError(query.error).message}
            </span>
          </SafeText>
          <Button small onClick={() => void query.refetch()}>
            {t("propsRetry")}
          </Button>
        </div>
      )}
      {query.isSuccess && (
        <PropTable
          rows={rows}
          total={items.length}
          busy={pending}
          onEdit={(r) => {
            setError(null);
            setEditing({ mode: "edit", name: r.name, value: r.value ?? "" });
          }}
          onDelete={(r) => void apply(r.name, null)}
        />
      )}
      {query.isSuccess && editing === null && (
        <div>
          <Button
            onClick={() => {
              setError(null);
              setEditing({ mode: "add" });
            }}
          >
            {t("propAdd")}
          </Button>
        </div>
      )}
      {editing && (
        <PropEditor
          key={editing.mode === "edit" ? editing.name : "add"}
          editing={editing}
          kind={kind}
          items={items}
          busy={pending}
          onCancel={() => {
            setError(null);
            setEditing(null);
          }}
          onSave={(name, value) => void apply(name, value)}
        />
      )}
      {error && (
        <SafeText as="p" className="m-0 break-words text-bad-text">
          <span role="alert">{error}</span>
        </SafeText>
      )}
      <p className="m-0 text-[12px] text-muted-text">{t("propLocalNote")}</p>
    </Dialog>
  );
}

function PropTable({
  rows,
  total,
  busy,
  onEdit,
  onDelete,
}: {
  rows: PropRow[];
  total: number;
  busy: boolean;
  onEdit: (r: PropRow) => void;
  onDelete: (r: PropRow) => void;
}) {
  const t = useT(propsStrings);
  if (rows.length === 0) return <p className="m-0 text-muted-text">{t("propsEmpty")}</p>;
  return (
    <div className="max-h-64 overflow-auto rounded-md border border-line">
      <table className="w-full border-collapse text-[12px]">
        <caption className="sr-only">{t("propsTable")}</caption>
        <thead className="bg-surface-2 text-left">
          <tr>
            <th scope="col" className="px-2 py-1 font-semibold">
              {t("propName")}
            </th>
            <th scope="col" className="px-2 py-1 font-semibold">
              {t("propValue")}
            </th>
            <th scope="col" className="px-2 py-1 font-semibold">
              <span className="sr-only">{t("propActions")}</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.name} className="border-t border-line align-top">
              <th scope="row" className="px-2 py-1 text-left font-mono font-normal">
                <SafeText>{r.name}</SafeText>
                {r.count < total && (
                  <span className="block text-muted-text">
                    {t("propOnSome", { count: r.count, total })}
                  </span>
                )}
              </th>
              <td className="max-w-0 px-2 py-1 font-mono">
                {r.binary ? (
                  <span className="text-muted-text">{t("propBinary")}</span>
                ) : r.value === null ? (
                  <span className="text-muted-text">{t("propVaries")}</span>
                ) : (
                  <SafeText
                    as="pre"
                    className="m-0 max-h-24 overflow-auto whitespace-pre-wrap break-words"
                  >
                    {r.value}
                  </SafeText>
                )}
              </td>
              <td className="whitespace-nowrap px-2 py-1 text-right">
                <span className="inline-flex gap-1">
                  <Button
                    small
                    disabled={busy || r.binary}
                    aria-label={t("propEditLabel", { name: r.name })}
                    onClick={() => onEdit(r)}
                  >
                    {t("propEdit")}
                  </Button>
                  <Button
                    small
                    disabled={busy}
                    aria-label={t("propDeleteLabel", { name: r.name })}
                    onClick={() => onDelete(r)}
                  >
                    {t("propDelete")}
                  </Button>
                </span>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

type PropsT = (key: keyof (typeof propsStrings)["en"], vars?: Vars) => string;

function problemText(t: PropsT, p: PropProblem) {
  switch (p.key) {
    case "propUnknownSvn":
    case "propFileOnly":
    case "propDirOnly":
      return t(p.key, { name: p.name });
    case "propMimeInvalid":
      return t(p.key, { detail: p.detail });
    case "propExternalsInvalid":
    case "propMergeinfoInvalid":
      return t(p.key, { line: p.line });
    default:
      return t(p.key);
  }
}

function PropEditor({
  editing,
  kind,
  items,
  busy,
  onCancel,
  onSave,
}: {
  editing: NonNullable<Editing>;
  kind: FileKind;
  items: ItemProps[];
  busy: boolean;
  onCancel: () => void;
  onSave: (name: string, value: string) => void;
}) {
  const t = useT(propsStrings);
  const listId = useId();
  const eolId = useId();
  const valueId = useId();
  const [name, setName] = useState(editing.mode === "edit" ? editing.name : "");
  const [value, setValue] = useState(editing.mode === "edit" ? editing.value : "");
  const [touched, setTouched] = useState(editing.mode === "edit");
  const trimmed = name.trim();
  const boolean = BOOLEAN_PROPS.includes(trimmed);
  // Checked against each item's own properties (an eol-style next to a binary mime-type).
  const problem =
    items
      .map((i) => propProblem(trimmed, boolean ? "*" : value, kind, i.props))
      .find((p) => p !== null) ?? (items.length === 0 ? propProblem(trimmed, value, kind) : null);
  const shown = touched || trimmed !== "" ? problem : null;
  const multiline = !boolean && trimmed !== "svn:eol-style" && trimmed !== "svn:mime-type";

  return (
    <form
      aria-label={editing.mode === "edit" ? t("propEditing") : t("propAdding")}
      className="flex flex-col gap-2 rounded-md border border-line p-2.5"
      onSubmit={(e) => {
        e.preventDefault();
        setTouched(true);
        if (problem === null && !busy) onSave(trimmed, boolean ? "*" : value);
      }}
    >
      <label className="flex flex-col gap-1">
        {t("propName")}
        <Input
          value={name}
          list={listId}
          readOnly={editing.mode === "edit"}
          aria-invalid={shown?.key.startsWith("propName") === true}
          spellCheck={false}
          autoComplete="off"
          onChange={(e) => {
            setName(e.target.value);
            setTouched(true);
          }}
          className="font-mono"
        />
        <datalist id={listId} aria-label={t("propSuggestions")}>
          {knownPropsFor(kind).map((p) => (
            <option key={p} value={p} />
          ))}
        </datalist>
      </label>
      {boolean ? (
        <p className="m-0 text-[12px] text-muted-text">{t("propBooleanNote")}</p>
      ) : (
        <div className="flex flex-col gap-1">
          <label htmlFor={valueId}>{t("propValue")}</label>
          {multiline ? (
            <Textarea
              id={valueId}
              rows={4}
              value={value}
              spellCheck={false}
              className="font-mono"
              onChange={(e) => setValue(e.target.value)}
            />
          ) : (
            <Input
              id={valueId}
              value={value}
              spellCheck={false}
              list={trimmed === "svn:eol-style" ? eolId : undefined}
              className="font-mono"
              onChange={(e) => setValue(e.target.value)}
            />
          )}
          <datalist id={eolId}>
            {EOL_STYLES.map((s) => (
              <option key={s} value={s} />
            ))}
          </datalist>
        </div>
      )}
      {shown && (
        <SafeText as="p" className="m-0 break-words text-bad-text">
          <span role="alert">{problemText(t, shown)}</span>
        </SafeText>
      )}
      <div className="flex justify-end gap-2">
        <Button onClick={onCancel}>{t("propCancelEdit")}</Button>
        <Button type="submit" variant="primary" disabled={busy || problem !== null}>
          {t("propSave")}
        </Button>
      </div>
    </form>
  );
}
