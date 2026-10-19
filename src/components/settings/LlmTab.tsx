import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import * as ipc from "../../api/ipc";
import type { ConnectionTest, LlmConfig, LlmProviderDto, VisionStatus } from "../../api/types";
import { toMessage as errMsg } from "../../shared/errors";
import { confirmRemove } from "../../shared/confirm";
import { Card, Field } from "./shared";
import { useDebouncedConfig } from "./useDebouncedConfig";
import {
  DEFAULT_CONTEXT_WINDOW,
  detectContextToken,
  emptyProviderForm,
  llmToPatch,
  parseNum,
  toLlmForm,
  visionLabel,
  type LlmForm,
  type ProviderForm,
} from "./forms";

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
function ModelPicker({ value, models, error, busy, onChange, onFocus, onBlur }: ModelPickerProps) {
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

// Endpoint host shown under a provider name (scheme stripped, path dropped).
function endpointHost(url: string): string {
  const after = url.trim().split("://")[1] ?? url.trim();
  return after.split("/")[0];
}

type ProviderPreset = "deepseek" | "openai" | "ollama";

const PROVIDER_PRESETS: Record<ProviderPreset, { name: string; baseUrl: string; model?: string }> = {
  deepseek: {
    name: "DeepSeek",
    baseUrl: "https://api.deepseek.com/v1",
    model: "deepseek-chat",
  },
  openai: {
    name: "OpenAI",
    baseUrl: "https://api.openai.com/v1",
    model: "gpt-4o-mini",
  },
  ollama: {
    name: "Ollama",
    baseUrl: "http://localhost:11434/v1",
  },
};

function isLocalEndpoint(url: string): boolean {
  return /^https?:\/\/(localhost|127\.0\.0\.1|\[::1\])([:/]|$)/i.test(url.trim());
}

function validateProviderInput(name: string, baseUrl: string, apiKey: string): string | null {
  if (!name) return "Укажи имя провайдера";
  if (!/^https?:\/\//i.test(baseUrl)) {
    return "Endpoint должен начинаться с http:// или https://";
  }
  if (!apiKey && !isLocalEndpoint(baseUrl)) {
    return "Укажи API-ключ (пусто разрешено только для localhost)";
  }
  return null;
}

export default function LlmTab({
  active,
  error,
  setError,
}: {
  active: boolean;
  error: string | null;
  setError: (message: string) => void;
}) {
  // Context window: true once the user typed in the field; auto-fill never
  // overrides a hand-edited value. Reset when the model id itself changes.
  const ctxUserEditedRef = useRef(false);
  const lastModelIdRef = useRef<string | null>(null);
  // One-shot auto-load of the model list once the active provider is complete.
  const modelsAutoLoadedRef = useRef(false);
  const modelsAutoTimer = useRef<number | undefined>(undefined);

  // A new model id re-arms the context-window auto-fill; edits of the field
  // itself (same model) keep the "user edited" flag.
  const onLlmLoaded = useCallback((next: LlmForm) => {
    if (lastModelIdRef.current !== null && lastModelIdRef.current !== next.model) {
      ctxUserEditedRef.current = false;
    }
    lastModelIdRef.current = next.model;
  }, []);

  const {
    form: llmForm,
    updateForm: updateLlmForm,
    reload: reloadLlmConfig,
    flush: flushLlmSave,
    focusField: focusLlmField,
    blurField: blurLlmField,
    focusedRef: llmFocusedRef,
  } = useDebouncedConfig<LlmForm>(
    "llm",
    (raw) => toLlmForm(raw as unknown as LlmConfig),
    llmToPatch,
    setError,
    onLlmLoaded,
  );

  // Provider profiles (Q-24): the list, the id being edited (null = new draft),
  // and the editor form. baseUrl/apiKey live here, not in the debounced llm form.
  const [providers, setProviders] = useState<LlmProviderDto[] | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [provForm, setProvForm] = useState<ProviderForm>({ ...emptyProviderForm });
  const [provError, setProvError] = useState<string | null>(null);
  const [provBusy, setProvBusy] = useState(false);
  const [revealKey, setRevealKey] = useState(false);

  const [llmModels, setLlmModels] = useState<string[] | null>(null);
  const [llmModelsError, setLlmModelsError] = useState<string | null>(null);
  const [llmTest, setLlmTest] = useState<ConnectionTest | null>(null);
  const [llmBusy, setLlmBusy] = useState<"test" | "models" | null>(null);
  const [ctxAutoFilled, setCtxAutoFilled] = useState<{ token: string; tokens: number } | null>(null);
  const [vision, setVision] = useState<VisionStatus | null>(null);
  const [visionBusy, setVisionBusy] = useState(false);

  const activeProvider = providers?.find((p) => p.active) ?? null;
  const activeMissingKey =
    activeProvider !== null &&
    !activeProvider.apiKey.trim() &&
    !isLocalEndpoint(activeProvider.baseUrl);
  const emptyProviderDraft =
    editingId === null &&
    !provForm.name.trim() &&
    !provForm.baseUrl.trim() &&
    !provForm.apiKey.trim();

  const loadProviders = useCallback(() => {
    void ipc
      .llmProvidersList()
      .then(setProviders)
      .catch((e) => setError(errMsg(e)));
  }, [setError]);

  const loadVision = useCallback(() => {
    void ipc
      .getVisionStatus()
      .then(setVision)
      .catch(() => setVision(null));
  }, []);

  // detect_vision reads llm.json from disk, so pending form edits must land
  // first (same ordering as runLlmAction).
  const probeVision = () => {
    setVisionBusy(true);
    flushLlmSave()
      .then(() => ipc.detectVision())
      .then(setVision)
      .catch((e) => setError(errMsg(e)))
      .finally(() => setVisionBusy(false));
  };

  const persistProviderForm = useCallback(async (): Promise<string | null> => {
    setProvError(null);
    const name = provForm.name.trim();
    const baseUrl = provForm.baseUrl.trim();
    const apiKey = provForm.apiKey.trim();
    const emptyDraft = emptyProviderDraft;
    if (!emptyDraft) {
      const invalid = validateProviderInput(name, baseUrl, apiKey);
      if (invalid) {
        setProvError(invalid);
        throw new Error(invalid);
      }
    }
    await flushLlmSave();
    if (emptyDraft) return activeProvider?.id ?? null;
    setProvBusy(true);
    try {
      const previousIds = new Set((providers ?? []).map((p) => p.id));
      let list =
        editingId === null
          ? await ipc.llmProviderAdd(name, baseUrl, apiKey)
          : await ipc.llmProviderUpdate(editingId, name, baseUrl, apiKey);
      let savedId = editingId;
      if (savedId === null) {
        const added = list.find((p) => !previousIds.has(p.id));
        if (!added) throw new Error("Не удалось определить сохранённого провайдера");
        savedId = added.id;
      }
      if (!list.some((p) => p.id === savedId && p.active)) {
        list = await ipc.llmProviderActivate(savedId);
      }
      const saved = list.find((p) => p.id === savedId);
      const changed =
        !activeProvider ||
        !saved ||
        activeProvider.baseUrl !== saved.baseUrl ||
        activeProvider.apiKey !== saved.apiKey;
      setProviders(list);
      setEditingId(savedId);
      setProvForm({ name, baseUrl, apiKey });
      if (changed) {
        modelsAutoLoadedRef.current = false;
        setLlmModels(null);
        setLlmModelsError(null);
      }
      return savedId;
    } catch (err) {
      setProvError(errMsg(err));
      throw err;
    } finally {
      setProvBusy(false);
    }
  }, [activeProvider, editingId, emptyProviderDraft, flushLlmSave, provForm, providers]);

  // test_connection/list_models read llm.json from disk (the active provider),
  // so the pending form must be written first and the write must land before
  // the command runs.
  const runLlmAction = useCallback(
    (action: "test" | "models", persistProvider = false) => {
      setLlmBusy(action);
      const prepared = persistProvider ? persistProviderForm() : flushLlmSave();
      prepared
        .then(() =>
          action === "test"
            ? ipc.testConnection().then(setLlmTest)
            : ipc
                .listModels()
                .then((ms) => {
                  setLlmModels(ms.map((m) => m.id));
                  setLlmModelsError(null);
                  modelsAutoLoadedRef.current = true;
                })
                .catch(() => {
                  setLlmModels(null);
                  setLlmModelsError(
                    "Endpoint не вернул список моделей — впиши название вручную (уточни у провайдера)",
                  );
                }),
        )
        .catch((e) => {
          if (action === "test") setLlmTest({ ok: false, message: errMsg(e), modelCount: 0 });
        })
        .finally(() => setLlmBusy(null));
    },
    [flushLlmSave, persistProviderForm],
  );

  // Auto-load the model list once, debounced, once the active provider has an
  // endpoint and key and nothing is loaded yet; a failure leaves manual retry.
  useEffect(() => {
    if (modelsAutoLoadedRef.current) return;
    const p = activeProvider;
    if (!p || !p.baseUrl.trim() || !p.apiKey.trim()) return;
    if (llmModels !== null || llmBusy !== null) return;
    window.clearTimeout(modelsAutoTimer.current);
    modelsAutoTimer.current = window.setTimeout(() => {
      if (!modelsAutoLoadedRef.current) runLlmAction("models");
    }, 800);
    return () => window.clearTimeout(modelsAutoTimer.current);
  }, [activeProvider?.baseUrl, activeProvider?.apiKey, llmModels, llmBusy, runLlmAction]);

  // Auto-fill the context window from a size token in the model id
  // ("…-128k", "…-1m") while the field is still empty or on the default.
  useEffect(() => {
    const f = llmForm;
    if (!f || llmFocusedRef.current === "contextWindow") return;
    const hit = detectContextToken(f.model);
    if (!hit || ctxUserEditedRef.current) return;
    const cur = parseNum(f.contextWindow);
    if (cur !== null && cur !== DEFAULT_CONTEXT_WINDOW) return;
    if (cur !== null && Math.round(cur) === hit.tokens) return;
    setCtxAutoFilled(hit);
    updateLlmForm({ contextWindow: String(hit.tokens) });
  }, [llmForm, updateLlmForm]);

  useEffect(() => {
    loadProviders();
    loadVision();
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void ipc
      .onConfigChanged(({ which }) => {
        if (which !== "llm") return;
        reloadLlmConfig();
        loadProviders();
        loadVision();
      })
      .then((u) => (cancelled ? u() : (unlisten = u)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [reloadLlmConfig, loadProviders, loadVision]);

  // Provider mutations flush the debounced llm form first so a stale form read
  // can never clobber the provider keys the command is about to write.
  const startNew = () => {
    setEditingId(null);
    setProvForm({ ...emptyProviderForm });
    setProvError(null);
    setRevealKey(false);
  };

  const startEdit = (p: LlmProviderDto) => {
    setEditingId(p.id);
    setProvForm({ name: p.name, baseUrl: p.baseUrl, apiKey: p.apiKey });
    setProvError(null);
    setRevealKey(false);
  };

  const activateProvider = async (id: string) => {
    if (id === activeProvider?.id) return;
    setProvBusy(true);
    setProvError(null);
    try {
      await flushLlmSave();
      setProviders(await ipc.llmProviderActivate(id));
      // A different endpoint: re-arm the model auto-load for the new provider.
      modelsAutoLoadedRef.current = false;
      setLlmModels(null);
      setLlmModelsError(null);
    } catch (e) {
      setProvError(errMsg(e));
    } finally {
      setProvBusy(false);
    }
  };

  const deleteProvider = async (p: LlmProviderDto) => {
    const ok = await confirmRemove({
      title: `Удалить провайдера «${p.name}»?`,
      message: "Вместе с провайдером будет удалён его API-ключ.",
    });
    if (!ok) return;
    setProvBusy(true);
    setProvError(null);
    try {
      await flushLlmSave();
      const list = await ipc.llmProviderDelete(p.id);
      setProviders(list);
      if (editingId === p.id) startNew();
    } catch (e) {
      setProvError(errMsg(e));
    } finally {
      setProvBusy(false);
    }
  };

  const saveProvider = async (e: FormEvent) => {
    e.preventDefault();
    setProvError(null);
    const name = provForm.name.trim();
    const baseUrl = provForm.baseUrl.trim();
    const apiKey = provForm.apiKey.trim();
    const invalid = validateProviderInput(name, baseUrl, apiKey);
    if (invalid) {
      setProvError(invalid);
      return;
    }
    setProvBusy(true);
    try {
      await flushLlmSave();
      const list =
        editingId === null
          ? await ipc.llmProviderAdd(name, baseUrl, apiKey)
          : await ipc.llmProviderUpdate(editingId, name, baseUrl, apiKey);
      setProviders(list);
      startNew();
    } catch (err) {
      setProvError(errMsg(err));
    } finally {
      setProvBusy(false);
    }
  };

  if (!active) return null;
  return (
    <div className="settings-content">
      <Card
        title="Провайдеры"
        desc="Сохраняй любое число OpenAI-совместимых провайдеров; активен ровно один — переключение только здесь."
      >
        {providers === null ? (
          <div className="set-empty">Загрузка…</div>
        ) : providers.length === 0 ? (
          <div className="set-empty">Провайдеров нет. Добавь первый ниже.</div>
        ) : (
          <div className="set-list">
            {providers.map((p) => (
              <div key={p.id} className="provider-item">
                <button
                  className="provider-radio"
                  role="radio"
                  aria-checked={p.active}
                  title={p.active ? "Активный провайдер" : "Сделать активным"}
                  disabled={provBusy}
                  onClick={() => void activateProvider(p.id)}
                >
                  <span className={p.active ? "dot on" : "dot"}></span>
                </button>
                <div className="info">
                  <div className="t">{p.name}</div>
                  <div className="c">{endpointHost(p.baseUrl)}</div>
                </div>
                {p.active && <span className="badge st-active">активен</span>}
                <span className="mcp-actions">
                  <button className="ghost-btn small" onClick={() => startEdit(p)}>
                    Изменить
                  </button>
                  <button className="ghost-btn small" onClick={() => void deleteProvider(p)}>
                    Удалить
                  </button>
                </span>
              </div>
            ))}
          </div>
        )}
        {activeMissingKey && (
          <div className="provider-warn">
            У активного провайдера нет API-ключа — впиши ключ и сохрани провайдера.
          </div>
        )}
        <div className="set-actions">
          <button className="primary-btn small" onClick={startNew} disabled={provBusy}>
            + Добавить провайдера
          </button>
        </div>
      </Card>
      <Card
        title={editingId === null ? "Новый провайдер" : "Подключение (LLM)"}
        desc="Имя, endpoint и API-ключ активного провайдера. Ключ хранится в llm.json в открытом виде (см. ТЗ 3.4)."
      >
        <form className="set-form" onSubmit={(e) => void saveProvider(e)}>
          <div className="set-hint">
            OpenAI-совместимые провайдеры используют Bearer-аутентификацию. Форма сохраняется
            автоматически при проверке подключения и загрузке моделей.
          </div>
          <Field
            label="Шаблон"
            wide
            hint="Заполнит имя, endpoint и модель. API-ключ останется как есть."
          >
            <select
              value=""
              onChange={(e) => {
                const preset = PROVIDER_PRESETS[e.target.value as ProviderPreset];
                if (!preset) return;
                setProvForm({ ...provForm, name: preset.name, baseUrl: preset.baseUrl });
                if (preset.model) updateLlmForm({ model: preset.model });
              }}
            >
              <option value="">Выбери шаблон…</option>
              <option value="deepseek">DeepSeek</option>
              <option value="openai">OpenAI</option>
              <option value="ollama">Ollama (локально)</option>
            </select>
          </Field>
          <Field label="Название" wide hint="Произвольная метка провайдера, видна в списке.">
            <input
              type="text"
              value={provForm.name}
              onChange={(e) => setProvForm({ ...provForm, name: e.target.value })}
              placeholder="DeepSeek"
              spellCheck={false}
            />
          </Field>
          <Field label="Адрес эндпоинта" wide hint="Базовый адрес провайдера модели, как у OpenAI.">
            <input
              type="text"
              value={provForm.baseUrl}
              onChange={(e) => setProvForm({ ...provForm, baseUrl: e.target.value })}
              placeholder="https://api.example.com/v1"
              spellCheck={false}
            />
          </Field>
          <Field label="API-ключ" wide hint="Пусто разрешено только для локального сервера (Ollama).">
            <div className="llm-key-row">
              <input
                type={revealKey ? "text" : "password"}
                value={provForm.apiKey}
                onChange={(e) => setProvForm({ ...provForm, apiKey: e.target.value })}
                autoComplete="off"
                spellCheck={false}
              />
              <button className="ghost-btn small" type="button" onClick={() => setRevealKey((v) => !v)}>
                {revealKey ? "Скрыть" : "Показать"}
              </button>
            </div>
          </Field>
          <div className="set-actions">
            <button className="primary-btn small" type="submit" disabled={provBusy}>
              {editingId === null ? "Добавить" : "Сохранить"}
            </button>
            {editingId !== null && (
              <button className="ghost-btn small" type="button" onClick={startNew}>
                Отмена
              </button>
            )}
            <button
              className="ghost-btn"
              type="button"
              disabled={
                llmBusy !== null || provBusy || (activeProvider === null && emptyProviderDraft)
              }
              onClick={() => void runLlmAction("test", true)}
            >
              {llmBusy === "test" && <span className="spinner"></span>}
              {llmBusy === "test" ? "Проверка…" : "Проверить подключение"}
            </button>
            {llmTest && (
              <div className={llmTest.ok ? "llm-test ok" : "llm-test err"}>
                <span className="mark">{llmTest.ok ? "✓" : "✕"}</span>
                {llmTest.message}
              </div>
            )}
          </div>
          {provError && <div className="set-error">{provError}</div>}
        </form>
      </Card>
      <Card title="Генерация" desc="Параметры запросов к модели. Общие для всех провайдеров.">
        <Field
          label="Модель"
          wide
          hint="Список подтягивается с endpoint’а активного провайдера."
        >
          <div className="model-row">
            <ModelPicker
              value={llmForm?.model ?? ""}
              models={llmModels}
              error={llmModelsError}
              busy={llmBusy === "models"}
              onChange={(model) => updateLlmForm({ model })}
              onFocus={focusLlmField("model")}
              onBlur={blurLlmField}
            />
            <button
              className="ghost-btn small"
              disabled={
                llmBusy !== null || provBusy || (activeProvider === null && emptyProviderDraft)
              }
              onClick={() => void runLlmAction("models", true)}
            >
              {llmBusy === "models" && <span className="spinner"></span>}
              {llmBusy === "models" ? "Загрузка…" : "Загрузить модели"}
            </button>
          </div>
        </Field>
        <Field
          label="Окно контекста, токенов"
          hint={
            ctxAutoFilled
              ? `Заполнено по названию модели (${ctxAutoFilled.token}) — можно исправить руками.`
              : detectContextToken(llmForm?.model ?? "")
                ? "В поле уже вписано значение — оно не меняется автоматически."
                : "Размер не угадывается по названию модели — заполни вручную."
          }
        >
          <input
            className="narrow"
            type="text"
            inputMode="numeric"
            value={llmForm?.contextWindow ?? ""}
            onChange={(e) => {
              ctxUserEditedRef.current = true;
              setCtxAutoFilled(null);
              updateLlmForm({ contextWindow: e.target.value });
            }}
            onFocus={focusLlmField("contextWindow")}
            onBlur={blurLlmField}
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
            value={llmForm?.maxTokens ?? ""}
            onChange={(e) => updateLlmForm({ maxTokens: e.target.value })}
            onFocus={focusLlmField("maxTokens")}
            onBlur={blurLlmField}
          />
        </Field>
        <Field
          label="Уровень размышлений"
          hint="Сколько модель думает перед ответом (reasoning_effort). Ход мыслей показывается свёрнутым блоком «Размышления» в чате и отправляется обратно модели в следующем запросе. Если провайдер не принимает reasoning_effort, ошибка придёт в чат."
        >
          <select
            value={llmForm?.thinkingLevel ?? "medium"}
            onChange={(e) => {
              updateLlmForm({
                thinkingLevel: e.target.value as "off" | "low" | "medium" | "high",
              });
              void flushLlmSave();
            }}
          >
            <option value="off">Выключено</option>
            <option value="low">Низкий</option>
            <option value="medium">Средний</option>
            <option value="high">Высокий</option>
          </select>
        </Field>
      </Card>
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
            value={llmForm?.vision ?? "auto"}
            onChange={(e) => {
              updateLlmForm({ vision: e.target.value as "auto" | "yes" | "no" });
              void flushLlmSave();
            }}
          >
            <option value="auto">Авто (определять)</option>
            <option value="yes">Да</option>
            <option value="no">Нет</option>
          </select>
        </Field>
        <div className="set-actions">
          <button className="ghost-btn" disabled={visionBusy} onClick={probeVision}>
            {visionBusy ? "Проверка…" : "Проверить"}
          </button>
        </div>
      </Card>
      {error && <div className="set-error">{error}</div>}
    </div>
  );
}
