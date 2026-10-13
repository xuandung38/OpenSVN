import { MergeView } from "@codemirror/merge";
import { EditorView } from "@codemirror/view";

/** Plain-text CodeMirror merge view (no HTML rendering), fixed-height so it scrolls internally. */
export function createMergeView(parent: HTMLElement, a: string, b: string): MergeView {
  const theme = EditorView.theme({
    "&": { height: "100%", fontFamily: "var(--f-mono)", fontSize: "12px" },
    ".cm-scroller": { overflow: "auto", lineHeight: "1.6" },
  });
  return new MergeView({
    a: { doc: a, extensions: [theme, EditorView.editable.of(false)] },
    b: { doc: b, extensions: [theme, EditorView.editable.of(false)] },
    parent,
    gutter: true,
    highlightChanges: true,
  });
}
