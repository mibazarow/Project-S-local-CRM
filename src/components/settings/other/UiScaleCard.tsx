import { Card, Field } from "../shared";
import Segmented from "../../shared/Segmented";
import { UI_SCALE_STEPS, uiScaleLabel } from "../../../shared/uiScale";

// UI scale (Q.md item 4): discrete steps, a live-applied webview zoom and a
// retry button while the last apply failed.
export function UiScaleCard({
  percent,
  scaleError,
  onScale,
}: {
  percent: number;
  scaleError: string | null;
  onScale: (percent: number) => void;
}) {
  return (
    <Card
      title="Интерфейс"
      desc="Масштаб всего интерфейса: сайдбар, чат, задачи, файлы и окна. Применяется сразу, без перезапуска."
    >
      <Field
        label="Масштаб"
        hint="Дискретные шаги от 100 % до 175 %."
        error={
          scaleError ? `Не удалось применить масштаб: ${scaleError}` : null
        }
      >
        <Segmented
          value={String(percent)}
          options={UI_SCALE_STEPS.map((step) => ({
            value: String(step),
            label: `${step}`,
          }))}
          onChange={(value) => onScale(Number(value))}
          title="Масштаб интерфейса"
        />
        <span className="scale-value">{uiScaleLabel(percent)}</span>
        <button
          type="button"
          className="ghost-btn small"
          onClick={() => onScale(100)}
          disabled={percent === 100}
        >
          Сбросить
        </button>
        {scaleError && (
          <button
            type="button"
            className="ghost-btn small"
            onClick={() => onScale(percent)}
          >
            Повторить
          </button>
        )}
      </Field>
    </Card>
  );
}
