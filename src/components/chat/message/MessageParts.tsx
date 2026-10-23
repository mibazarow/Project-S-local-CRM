import { MarkdownMessage } from "../MarkdownMessage";
import type { Compaction, Step } from "../chatTypes";

function StepRow({ step }: { step: Step }) {
  return (
    <div className={`tool-step ${step.status === "err" ? "err" : ""}`}>
      <span className="dot" />
      <span className="nm">{step.name}</span>
      <span className="args">{step.summary}</span>
      <span className="res">{step.status === "run" ? "…" : step.status === "ok" ? "ok" : "ош."}</span>
    </div>
  );
}

export function StepsList({ steps }: { steps: Step[] }) {
  if (steps.length === 0) return null;
  return (
    <details className="tool-steps">
      <summary>Действия агента ({steps.length})</summary>
      {steps.map((st) => (
        <StepRow key={st.key} step={st} />
      ))}
    </details>
  );
}

// Thinking trace of an agent turn: a collapsed «Размышления» block above the
// answer, Markdown-rendered, live while the run streams.
export function ReasoningBlock({ text }: { text: string }) {
  if (!text.trim()) return null;
  return (
    <details className="reasoning">
      <summary>Размышления</summary>
      <div className="reasoning-body">
        <MarkdownMessage text={text} />
      </div>
    </details>
  );
}

// Context-compaction status item: a collapsed «Лимит контекста — делаю
// резюме» line; expanding it reveals the checkpoint summary. Collapsed by
// default, it never dumps the summary inline.
export function CompactionNote({ compaction }: { compaction: Compaction }) {
  if (compaction.failed) {
    return (
      <details className="compaction failed">
        <summary>Не удалось сжать контекст</summary>
        <div className="compaction-body">{compaction.summary}</div>
      </details>
    );
  }
  return (
    <details className="compaction">
      <summary>Лимит контекста — делаю резюме</summary>
      <div className="compaction-body">
        <MarkdownMessage text={compaction.summary} />
      </div>
    </details>
  );
}
