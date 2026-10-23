import { type FormEvent } from "react";
import type { ConnectionTest } from "../../../api/types";
import { Card, Field } from "../shared";
import { type ProviderForm } from "../forms";
import { type ProviderPreset } from "./presets";

export function ProviderFormCard({
  form,
  onFormChange,
  editingId,
  busy,
  error,
  revealKey,
  onToggleRevealKey,
  test,
  testBusy,
  actionsDisabled,
  onApplyPreset,
  onTest,
  onSubmit,
  onCancel,
}: {
  form: ProviderForm;
  onFormChange: (form: ProviderForm) => void;
  editingId: string | null;
  busy: boolean;
  error: string | null;
  revealKey: boolean;
  onToggleRevealKey: () => void;
  test: ConnectionTest | null;
  testBusy: boolean;
  actionsDisabled: boolean;
  onApplyPreset: (preset: ProviderPreset) => void;
  onTest: () => void;
  onSubmit: (e: FormEvent) => void;
  onCancel: () => void;
}) {
  return (
    <Card
      title={editingId === null ? "Новый провайдер" : "Подключение (LLM)"}
      desc="Имя, endpoint и API-ключ активного провайдера. Ключ хранится в llm.json в открытом виде (см. ТЗ 3.4)."
    >
      <form className="set-form" onSubmit={(e) => void onSubmit(e)}>
        <div className="set-hint">
          OpenAI-совместимые провайдеры используют Bearer-аутентификацию. Форма сохраняется
          автоматически при проверке подключения и загрузке моделей.
        </div>
        <Field
          label="Шаблон"
          wide
          hint="Заполнит имя, endpoint и модель. API-ключ останется как есть."
        >
          <select value="" onChange={(e) => onApplyPreset(e.target.value as ProviderPreset)}>
            <option value="">Выбери шаблон…</option>
            <option value="deepseek">DeepSeek</option>
            <option value="openai">OpenAI</option>
            <option value="ollama">Ollama (локально)</option>
          </select>
        </Field>
        <Field label="Название" wide hint="Произвольная метка провайдера, видна в списке.">
          <input
            type="text"
            value={form.name}
            onChange={(e) => onFormChange({ ...form, name: e.target.value })}
            placeholder="DeepSeek"
            spellCheck={false}
          />
        </Field>
        <Field label="Адрес эндпоинта" wide hint="Базовый адрес провайдера модели, как у OpenAI.">
          <input
            type="text"
            value={form.baseUrl}
            onChange={(e) => onFormChange({ ...form, baseUrl: e.target.value })}
            placeholder="https://api.example.com/v1"
            spellCheck={false}
          />
        </Field>
        <Field label="API-ключ" wide hint="Пусто разрешено только для локального сервера (Ollama).">
          <div className="llm-key-row">
            <input
              type={revealKey ? "text" : "password"}
              value={form.apiKey}
              onChange={(e) => onFormChange({ ...form, apiKey: e.target.value })}
              autoComplete="off"
              spellCheck={false}
            />
            <button className="ghost-btn small" type="button" onClick={onToggleRevealKey}>
              {revealKey ? "Скрыть" : "Показать"}
            </button>
          </div>
        </Field>
        <div className="set-actions">
          <button className="primary-btn small" type="submit" disabled={busy}>
            {editingId === null ? "Добавить" : "Сохранить"}
          </button>
          {editingId !== null && (
            <button className="ghost-btn small" type="button" onClick={onCancel}>
              Отмена
            </button>
          )}
          <button
            className="ghost-btn"
            type="button"
            disabled={actionsDisabled}
            onClick={onTest}
          >
            {testBusy && <span className="spinner"></span>}
            {testBusy ? "Проверка…" : "Проверить подключение"}
          </button>
          {test && (
            <div className={test.ok ? "llm-test ok" : "llm-test err"}>
              <span className="mark">{test.ok ? "✓" : "✕"}</span>
              {test.message}
            </div>
          )}
        </div>
        {error && <div className="set-error">{error}</div>}
      </form>
    </Card>
  );
}
