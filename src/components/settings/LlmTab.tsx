import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type KeyboardEvent,
} from "react";
import * as ipc from "../../api/ipc";
import type { ConnectionTest, LlmConfig, VisionStatus } from "../../api/types";
import { toMessage as errMsg } from "../../shared/errors";
import { Card, Field } from "./shared";
import { useDebouncedConfig } from "./useDebouncedConfig";
import {
  DEFAULT_CONTEXT_WINDOW,
  detectContextToken,
  llmToPatch,
  parseNum,
  toLlmForm,
  visionLabel,
  type LlmForm,
} from "./forms";

// Searchable model combobox: free-text id + filtered dropdown of ids loaded
// from the endpoint. ↑/↓ move the highlight, Enter picks it, Esc closes.
interface ModelComboboxProps {
  value: string;
  models: string[] | null;
  onChange: (value: string) => void;
  onFocus: () => void;
  onBlur: () => void;
}

function ModelCombobox({ value, models, onChange, onFocus, onBlur }: ModelComboboxProps) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const listRef = useRef<HTMLDivElement | null>(null);

  const query = value.trim().toLowerCase();
  const filtered = (models ?? []).filter((id) => id.toLowerCase().includes(query));

  useEffect(() => {
    if (!open || active < 0) return;
    const node = listRef.current?.children[active] as HTMLElement | undefined;
    node?.scrollIntoView({ block: "nearest" });
  }, [open, active]);

  const pick = (id: string) => {
    onChange(id);
    setOpen(false);
    setActive(-1);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!open) {
        if (filtered.length > 0) setOpen(true);
        return;
      }
      const next =
        e.key === "ArrowDown"
          ? (active + 1) % filtered.length
          : (active - 1 + filtered.length) % filtered.length;
      setActive(filtered.length ? next : -1);
    } else if (e.key === "Enter") {
      if (open && active >= 0 && active < filtered.length) {
        e.preventDefault();
        pick(filtered[active]);
      } else {
        setOpen(false);
      }
    } else if (e.key === "Escape") {
      e.stopPropagation();
      setOpen(false);
      setActive(-1);
    }
  };

  return (
    <div className="model-combobox">
      <input
        type="text"
        role="combobox"
        aria-expanded={open}
        autoComplete="off"
        spellCheck={false}
        placeholder="gpt-4o-mini или впиши название вручную"
        value={value}
        onChange={(e) => {
          onChange(e.target.value);
          setOpen(true);
          setActive(-1);
        }}
        onFocus={() => {
          onFocus();
          setOpen(true);
          setActive(-1);
        }}
        onBlur={() => {
          onBlur();
          setOpen(false);
          setActive(-1);
        }}
        onKeyDown={onKeyDown}
      />
      {open && (
        <div className="model-dropdown" ref={listRef}>
          {filtered.length === 0 ? (
            <div className="model-empty">
              {models && models.length > 0
                ? "Ничего не найдено — можно вписать вручную"
                : "Список моделей ещё не загружен"}
            </div>
          ) : (
            filtered.map((id, i) => (
              <div
                key={id}
                className={i === active ? "model-option active" : "model-option"}
                // mousedown keeps focus in the input so blur does not close
                // the dropdown before click lands
                onMouseDown={(e) => e.preventDefault()}
                onMouseEnter={() => setActive(i)}
                onClick={() => pick(id)}
              >
                {id}
              </div>
            ))
          )}
        </div>
      )}
    </div>
  );
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
  // One-shot auto-load of the model list once endpoint+key are filled in.
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

  const [llmModels, setLlmModels] = useState<string[] | null>(null);
  const [llmModelsError, setLlmModelsError] = useState<string | null>(null);
  const [llmTest, setLlmTest] = useState<ConnectionTest | null>(null);
  const [llmBusy, setLlmBusy] = useState<"test" | "models" | null>(null);
  const [ctxAutoFilled, setCtxAutoFilled] = useState<{ token: string; tokens: number } | null>(null);
  const [revealKey, setRevealKey] = useState(false);
  const [vision, setVision] = useState<VisionStatus | null>(null);
  const [visionBusy, setVisionBusy] = useState(false);

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

  // test_connection/list_models read llm.json from disk, so the pending form
  // must be written first and the write must land before the command runs.
  const runLlmAction = useCallback(
    (action: "test" | "models") => {
      setLlmBusy(action);
      flushLlmSave()
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
    [flushLlmSave],
  );

  // Auto-load the model list once, debounced, after endpoint+key are filled
  // in and nothing is loaded yet; a failure leaves the manual retry button.
  useEffect(() => {
    const f = llmForm;
    if (!f || modelsAutoLoadedRef.current) return;
    if (!f.baseUrl.trim() || !f.apiKey.trim()) return;
    if (llmModels !== null || llmBusy !== null) return;
    window.clearTimeout(modelsAutoTimer.current);
    modelsAutoTimer.current = window.setTimeout(() => {
      if (!modelsAutoLoadedRef.current) runLlmAction("models");
    }, 800);
    return () => window.clearTimeout(modelsAutoTimer.current);
  }, [llmForm?.baseUrl, llmForm?.apiKey, llmModels, llmBusy, runLlmAction]);

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
    loadVision();
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void ipc
      .onConfigChanged(({ which }) => {
        if (which !== "llm") return;
        reloadLlmConfig();
        loadVision();
      })
      .then((u) => (cancelled ? u() : (unlisten = u)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [reloadLlmConfig, loadVision]);

  if (!active) return null;
  return (
    <div className="settings-content">
      <Card
        title="Подключение"
        desc="Endpoint и ключ OpenAI-совместимого провайдера. Изменения сохраняются автоматически."
      >
        <Field label="Endpoint" wide hint="Базовый адрес провайдера модели, как у OpenAI.">
          <input
            type="text"
            value={llmForm?.baseUrl ?? ""}
            onChange={(e) => updateLlmForm({ baseUrl: e.target.value })}
            onFocus={focusLlmField("baseUrl")}
            onBlur={blurLlmField}
            placeholder="https://api.example.com/v1"
          />
        </Field>
        <Field label="API-ключ" wide hint="Хранится в llm.json в открытом виде (см. ТЗ 3.4).">
          <div className="llm-key-row">
            <input
              type={revealKey ? "text" : "password"}
              value={llmForm?.apiKey ?? ""}
              onChange={(e) => updateLlmForm({ apiKey: e.target.value })}
              onFocus={focusLlmField("apiKey")}
              onBlur={blurLlmField}
              autoComplete="off"
              spellCheck={false}
            />
            <button className="ghost-btn small" onClick={() => setRevealKey((v) => !v)}>
              {revealKey ? "Скрыть" : "Показать"}
            </button>
          </div>
        </Field>
        <Field
          label="Модель"
          wide
          hint="Список подтягивается с endpoint’а; можно вписать любое название вручную."
          error={llmModelsError}
        >
          <div className="model-row">
            <ModelCombobox
              value={llmForm?.model ?? ""}
              models={llmModels}
              onChange={(model) => updateLlmForm({ model })}
              onFocus={focusLlmField("model")}
              onBlur={blurLlmField}
            />
            <button
              className="ghost-btn small"
              disabled={llmBusy !== null}
              onClick={() => runLlmAction("models")}
            >
              {llmBusy === "models" && <span className="spinner"></span>}
              {llmBusy === "models" ? "Загрузка…" : "Загрузить модели"}
            </button>
          </div>
        </Field>
        <div className="set-actions">
          <button
            className="ghost-btn"
            disabled={llmBusy !== null}
            onClick={() => runLlmAction("test")}
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
      </Card>
      <Card title="Генерация" desc="Параметры запросов к модели.">
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
