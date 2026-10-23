import type { VisionStatus } from "../../../api/types";
import { Card, Field } from "../shared";
import { visionLabel, type LlmForm } from "../forms";

export function VisionCard({
  form,
  vision,
  visionBusy,
  onVisionChange,
  onProbe,
}: {
  form: LlmForm | null;
  vision: VisionStatus | null;
  visionBusy: boolean;
  onVisionChange: (vision: "auto" | "yes" | "no") => void;
  onProbe: () => void;
}) {
  return (
    <Card
      title="Распознавание изображений (vision)"
      desc={
        <>
          Статус: {visionLabel(vision)}
          {vision?.overridden ? " (задано вручную)" : ""}
          {vision?.reason ? ` — ${vision.reason}` : ""}
        </>
      }
    >
      <Field
        label="Отправлять изображения модели"
        hint="Если провайдер не поддерживает vision, изображения из чата не будут отправлены модели."
      >
        <select
          value={form?.vision ?? "auto"}
          onChange={(e) => onVisionChange(e.target.value as "auto" | "yes" | "no")}
        >
          <option value="auto">Авто (определять)</option>
          <option value="yes">Да</option>
          <option value="no">Нет</option>
        </select>
      </Field>
      <div className="set-actions">
        <button className="ghost-btn" disabled={visionBusy} onClick={onProbe}>
          {visionBusy ? "Проверка…" : "Проверить"}
        </button>
      </div>
    </Card>
  );
}
