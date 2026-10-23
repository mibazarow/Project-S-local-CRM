import { Card, Field } from "../shared";
import {
  clampContextWindow,
  parseNum,
  type LlmForm,
} from "../forms";
import { ModelPicker } from "./ModelPicker";

export function GenerationCard({
  form,
  modelIds,
  modelsError,
  modelsBusy,
  actionsDisabled,
  selectedMax,
  onModelChange,
  onModelsLoad,
  onUpdate,
  onFocusField,
  onBlurField,
  onThinkingChange,
}: {
  form: LlmForm | null;
  modelIds: string[] | null;
  modelsError: string | null;
  modelsBusy: boolean;
  actionsDisabled: boolean;
  selectedMax: number | null;
  onModelChange: (model: string) => void;
  onModelsLoad: () => void;
  onUpdate: (patch: Partial<LlmForm>) => void;
  onFocusField: (name: keyof LlmForm) => () => void;
  onBlurField: () => void;
  onThinkingChange: (level: "off" | "low" | "medium" | "high") => void;
}) {
  return (
    <Card title="Генерация" desc="Параметры запросов к модели. Общие для всех провайдеров.">
      <Field
        label="Модель"
        wide
        hint="Список подтягивается с endpoint’а активного провайдера."
      >
        <div className="model-row">
          <ModelPicker
            value={form?.model ?? ""}
            models={modelIds}
            error={modelsError}
            busy={modelsBusy}
            onChange={onModelChange}
            onFocus={onFocusField("model")}
            onBlur={onBlurField}
          />
          <button
            className="ghost-btn small"
            disabled={actionsDisabled}
            onClick={onModelsLoad}
          >
            {modelsBusy && <span className="spinner"></span>}
            {modelsBusy ? "Загрузка…" : "Загрузить модели"}
          </button>
        </div>
      </Field>
      <Field
        label="Окно контекста, токенов"
        hint={
          selectedMax != null
            ? `Максимум провайдера для этой модели — ${selectedMax.toLocaleString("ru-RU")} токенов. Можно уменьшить, но не больше максимума.`
            : "Провайдер не сообщил максимум для этой модели: новое значение по умолчанию — 100 000 токенов, допустимо до 250 000."
        }
      >
        <input
          className="narrow"
          type="text"
          inputMode="numeric"
          value={form?.contextWindow ?? ""}
          onChange={(e) => onUpdate({ contextWindow: e.target.value })}
          onFocus={onFocusField("contextWindow")}
          onBlur={(e) => {
            const n = parseNum(e.currentTarget.value);
            if (n !== null) {
              onUpdate({ contextWindow: String(clampContextWindow(n, selectedMax)) });
            }
            onBlurField();
          }}
        />
      </Field>
      <Field
        label="Temperature"
        hint="Фиксирована для каждого агента и не настраивается: проекты и сессии харнеса — 0.2, заметки — 0.4. Поле в llm.json игнорируется."
      />
      <Field
        label="Максимум токенов в ответе"
        hint="Пусто — без ограничения со стороны приложения."
      >
        <input
          className="narrow"
          type="text"
          inputMode="numeric"
          value={form?.maxTokens ?? ""}
          onChange={(e) => onUpdate({ maxTokens: e.target.value })}
          onFocus={onFocusField("maxTokens")}
          onBlur={onBlurField}
        />
      </Field>
      <Field
        label="Уровень размышлений"
        hint="Сколько модель думает перед ответом (reasoning_effort). Ход мыслей показывается свёрнутым блоком «Размышления» в чате и отправляется обратно модели в следующем запросе. Если провайдер не принимает reasoning_effort, ошибка придёт в чат."
      >
        <select
          value={form?.thinkingLevel ?? "medium"}
          onChange={(e) => onThinkingChange(e.target.value as "off" | "low" | "medium" | "high")}
        >
          <option value="off">Выключено</option>
          <option value="low">Низкий</option>
          <option value="medium">Средний</option>
          <option value="high">Высокий</option>
        </select>
      </Field>
    </Card>
  );
}
