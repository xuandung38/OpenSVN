import { MergeView, unifiedMergeView } from "@codemirror/merge";
import { EditorState, type Extension } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { useEffect, useRef } from "react";

const THEME = EditorView.theme({
  "&": { height: "100%", fontFamily: "var(--f-mono)", fontSize: "12px", color: "var(--fg)" },
  ".cm-scroller": { overflow: "auto", lineHeight: "1.6" },
  ".cm-content": { caretColor: "var(--fg)" },
  ".cm-gutters": {
    backgroundColor: "var(--surface-2)",
    color: "var(--muted-text)",
    borderRight: "1px solid var(--line)",
  },
  ".cm-merge-a .cm-changedLine, .cm-deletedChunk": { backgroundColor: "var(--del-bg)" },
  ".cm-merge-b .cm-changedLine, .cm-insertedLine": { backgroundColor: "var(--add-bg)" },
  ".cm-changedText": { textDecoration: "underline" },
});

/**
 * Read-only CodeMirror diff. File content is handed over as document text only, never as HTML, so a
 * hostile file cannot inject markup. The content stays focusable (read-only, not disabled) so
 * keyboard users can scroll and select.
 */
export function DiffMergeEditor({
  left,
  right,
  mode,
  label,
}: {
  left: string;
  right: string;
  mode: "split" | "unified";
  /** Accessible name of the editor. */
  label: string;
}) {
  const host = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const parent = host.current;
    if (!parent) return;
    const base: Extension[] = [
      THEME,
      EditorState.readOnly.of(true),
      EditorView.contentAttributes.of({ "aria-label": label, "aria-readonly": "true" }),
    ];
    if (mode === "split") {
      const view = new MergeView({
        a: { doc: left, extensions: base },
        b: { doc: right, extensions: base },
        parent,
        gutter: true,
        highlightChanges: true,
        collapseUnchanged: { margin: 3, minSize: 8 },
      });
      return () => view.destroy();
    }
    const view = new EditorView({
      parent,
      state: EditorState.create({
        doc: right,
        extensions: [
          ...base,
          unifiedMergeView({
            original: left,
            gutter: true,
            highlightChanges: true,
            mergeControls: false,
            collapseUnchanged: { margin: 3, minSize: 8 },
          }),
        ],
      }),
    });
    return () => view.destroy();
  }, [left, right, mode, label]);

  return <div ref={host} className="h-full min-h-0 overflow-hidden" />;
}
