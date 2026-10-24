import { useEffect, useRef, type RefObject } from "react";
import { basicSetup } from "codemirror";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { markdown } from "@codemirror/lang-markdown";

interface Props {
  value: string;
  onChange: (value: string) => void;
  autoFocus?: boolean;
  ariaLabel?: string;
  className?: string;
  viewRef?: RefObject<EditorView | null>;
}

// Light theme matching the macOS UI tokens; basicSetup already ships light
// syntax highlighting, so only surfaces/fonts/caret are overridden here.
const macTheme = EditorView.theme({
  "&": {
    backgroundColor: "var(--surface)",
    color: "var(--text)",
    fontSize: "var(--fs-control)",
  },
  ".cm-scroller": {
    fontFamily: "var(--font-mono)",
    lineHeight: "1.6",
  },
  ".cm-content": { padding: "var(--sp-4) var(--sp-4)" },
  ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--accent)" },
  "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground":
    { backgroundColor: "var(--accent-soft)" },
  ".cm-activeLine, .cm-activeLineGutter": { backgroundColor: "transparent" },
  ".cm-gutters": {
    backgroundColor: "var(--surface)",
    color: "var(--text-muted)",
    border: "none",
  },
});

// Obsidian-like Markdown source editor (CodeMirror 6, the engine Obsidian
// itself uses). The view is created once on mount; later `value` changes are
// only pushed into the doc when it differs from the editor content, so typing
// never resets the caret or the undo history.
export default function MarkdownCodeEditor({
  value,
  onChange,
  autoFocus = false,
  ariaLabel,
  className,
  viewRef,
}: Props) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const viewStateRef = useRef<EditorView | null>(null);
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const valueRef = useRef(value);
  valueRef.current = value;

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: valueRef.current,
        extensions: [
          basicSetup,
          markdown(),
          EditorView.lineWrapping,
          macTheme,
          EditorView.updateListener.of((update) => {
            if (update.docChanged) onChangeRef.current(update.state.doc.toString());
          }),
        ],
      }),
    });
    if (ariaLabel) view.contentDOM.setAttribute("aria-label", ariaLabel);
    if (autoFocus) view.focus();
    viewStateRef.current = view;
    if (viewRef) viewRef.current = view;
    return () => {
      if (viewRef) viewRef.current = null;
      viewStateRef.current = null;
      view.destroy();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const view = viewStateRef.current;
    if (!view || view.state.doc.toString() === value) return;
    view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: value } });
  }, [value]);

  return (
    <div
      ref={hostRef}
      className={className ? `md-code-editor ${className}` : "md-code-editor"}
    />
  );
}
