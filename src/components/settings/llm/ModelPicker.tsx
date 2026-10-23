const NO_MODELS_MESSAGE =
  "К сожалению, ваш endpoint не вернул список доступных моделей. Внесите название вручную. Уточните у провайдера.";

interface ModelPickerProps {
  value: string;
  models: string[] | null;
  error: string | null;
  busy: boolean;
  onChange: (value: string) => void;
  onFocus: () => void;
  onBlur: () => void;
}

// Loaded models are a closed list: the user picks one with a native select.
// Only an empty or failed model-list response falls back to a manual id.
export function ModelPicker({ value, models, error, busy, onChange, onFocus, onBlur }: ModelPickerProps) {
  if (models === null && !error) {
    return (
      <select className="model-picker" disabled value="">
        <option value="">{busy ? "Загрузка моделей…" : "Модели не загружены"}</option>
      </select>
    );
  }

  if (models === null || models.length === 0) {
    return (
      <div className="model-picker">
        <input
          type="text"
          autoComplete="off"
          spellCheck={false}
          placeholder="Название модели"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          onFocus={onFocus}
          onBlur={onBlur}
        />
        <div className="set-field-hint">{NO_MODELS_MESSAGE}</div>
      </div>
    );
  }

  const current = value.trim();
  const missing = current !== "" && !models.includes(current);

  return (
    <select
      className="model-picker"
      value={current}
      onChange={(e) => onChange(e.target.value)}
      onFocus={onFocus}
      onBlur={onBlur}
    >
      {current === "" && (
        <option value="" disabled>
          Выбери модель
        </option>
      )}
      {missing && <option value={current}>{current} (текущая)</option>}
      {models.map((id) => (
        <option key={id} value={id}>
          {id}
        </option>
      ))}
    </select>
  );
}
