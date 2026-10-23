import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import * as ipc from "../../api/ipc";
import type { ConnectionTest, LlmConfig, LlmProviderDto, ModelInfo, VisionStatus } from "../../api/types";
import { toMessage as errMsg } from "../../shared/errors";
import { confirmRemove } from "../../shared/confirm";
import { useDebouncedConfig } from "./useDebouncedConfig";
import {
  DEFAULT_CONTEXT_WINDOW,
  clampContextWindow,
  emptyProviderForm,
  llmToPatch,
  toLlmForm,
  type LlmForm,
  type ProviderForm,
} from "./forms";
import {
  PROVIDER_PRESETS,
  isLocalEndpoint,
  validateProviderInput,
  type ProviderPreset,
} from "./llm/presets";
import { ProviderListCard } from "./llm/ProviderListCard";
import { ProviderFormCard } from "./llm/ProviderFormCard";
import { GenerationCard } from "./llm/GenerationCard";
import { VisionCard } from "./llm/VisionCard";

export default function LlmTab({
  active,
  error,
  setError,
}: {
  active: boolean;
  error: string | null;
  setError: (message: string) => void;
}) {
  const lastModelIdRef = useRef<string | null>(null);
  // One-shot auto-load of the model list once the active provider is complete.
  const modelsAutoLoadedRef = useRef(false);
  const modelsAutoTimer = useRef<number | undefined>(undefined);
  // Provider id awaiting a one-shot model auto-pick after its list arrives.
  const pendingModelPickRef = useRef<string | null>(null);

  const {
    form: llmForm,
    updateForm: updateLlmForm,
    reload: reloadLlmConfig,
    flush: flushLlmSave,
    focusField: focusLlmField,
    blurField: blurLlmField,
  } = useDebouncedConfig<LlmForm>(
    "llm",
    (raw) => toLlmForm(raw as unknown as LlmConfig),
    llmToPatch,
    setError,
  );

  // Provider profiles (Q-24): the list, the id being edited (null = new draft),
  // and the editor form. baseUrl/apiKey live here, not in the debounced llm form.
  const [providers, setProviders] = useState<LlmProviderDto[] | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [provForm, setProvForm] = useState<ProviderForm>({ ...emptyProviderForm });
  const [provError, setProvError] = useState<string | null>(null);
  const [provBusy, setProvBusy] = useState(false);
  const [revealKey, setRevealKey] = useState(false);

  const [llmModels, setLlmModels] = useState<ModelInfo[] | null>(null);
  const [llmModelsError, setLlmModelsError] = useState<string | null>(null);
  const [llmTest, setLlmTest] = useState<ConnectionTest | null>(null);
  const [llmBusy, setLlmBusy] = useState<"test" | "models" | null>(null);
  const [vision, setVision] = useState<VisionStatus | null>(null);
  const [visionBusy, setVisionBusy] = useState(false);

  const modelIds = llmModels?.map((m) => m.id) ?? null;
  const selectedModel = llmForm?.model ?? "";
  const selectedMax =
    llmModels?.find((m) => m.id === selectedModel)?.maxContext ??
    llmForm?.modelContextMax[selectedModel] ??
    null;
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
  // Both endpoint commands need a saved provider and an idle busy state.
  const actionsDisabled =
    llmBusy !== null || provBusy || (activeProvider === null && emptyProviderDraft);

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
                  setLlmModels(ms);
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

  // After a provider switch, once its model list arrives: pick the remembered
  // model if it is in the list, otherwise the first one, and remember it.
  // Runs once per activation; a manual pick afterwards is never overwritten.
  useEffect(() => {
    const p = activeProvider;
    if (!p || pendingModelPickRef.current !== p.id) return;
    if (modelIds === null || modelIds.length === 0 || !llmForm) return;
    pendingModelPickRef.current = null;
    const remembered = llmForm.lastModelByProvider[p.id];
    const chosen = remembered && modelIds.includes(remembered) ? remembered : modelIds[0];
    updateLlmForm({
      model: chosen,
      lastModelByProvider: { ...llmForm.lastModelByProvider, [p.id]: chosen },
    });
  }, [modelIds, activeProvider, llmForm, updateLlmForm]);

  // Seed the selected model's own window: the provider-reported maximum wins,
  // an existing per-model value is kept (and clamped), and a newly selected
  // model with no reported maximum starts at 100k.
  useEffect(() => {
    const f = llmForm;
    if (!f || !f.model) return;
    const model = f.model;
    const reported = llmModels?.find((m) => m.id === model)?.maxContext ?? null;
    const persistedMax = f.modelContextMax[model] ?? null;
    const max = reported ?? persistedMax;
    const modelChanged = lastModelIdRef.current !== null && lastModelIdRef.current !== model;
    lastModelIdRef.current = model;
    const existing = f.modelContextWindows[model];
    if (existing !== undefined) {
      if (max != null && existing > max) {
        const clamped = clampContextWindow(existing, max);
        updateLlmForm({
          contextWindow: String(clamped),
          modelContextWindows: { ...f.modelContextWindows, [model]: clamped },
          modelContextMax: { ...f.modelContextMax, [model]: max },
        });
      } else if (max != null && persistedMax !== max) {
        updateLlmForm({ modelContextMax: { ...f.modelContextMax, [model]: max } });
      }
      return;
    }
    if (max != null) {
      const window = clampContextWindow(max, max);
      updateLlmForm({
        contextWindow: String(window),
        modelContextWindows: { ...f.modelContextWindows, [model]: window },
        modelContextMax: { ...f.modelContextMax, [model]: max },
      });
      return;
    }
    if (modelChanged) {
      updateLlmForm({
        contextWindow: String(DEFAULT_CONTEXT_WINDOW),
        modelContextWindows: {
          ...f.modelContextWindows,
          [model]: DEFAULT_CONTEXT_WINDOW,
        },
      });
    }
  }, [llmForm, llmModels, updateLlmForm]);

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
      pendingModelPickRef.current = id;
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

  const applyPreset = (value: ProviderPreset) => {
    const preset = PROVIDER_PRESETS[value];
    if (!preset) return;
    setProvForm({ ...provForm, name: preset.name, baseUrl: preset.baseUrl });
    if (preset.model) updateLlmForm({ model: preset.model });
  };

  // A pick from a loaded list is remembered per provider; manual entry into an
  // empty list only sets the model.
  const selectModel = (model: string) => {
    if (activeProvider && modelIds !== null && modelIds.length > 0) {
      updateLlmForm({
        model,
        lastModelByProvider: {
          ...(llmForm?.lastModelByProvider ?? {}),
          [activeProvider.id]: model,
        },
      });
    } else {
      updateLlmForm({ model });
    }
  };

  // Both selects below own their file: the debounced write is flushed at once.
  const setThinkingLevel = (level: "off" | "low" | "medium" | "high") => {
    updateLlmForm({ thinkingLevel: level });
    void flushLlmSave();
  };

  const setVisionChoice = (value: "auto" | "yes" | "no") => {
    updateLlmForm({ vision: value });
    void flushLlmSave();
  };

  if (!active) return null;
  return (
    <div className="settings-content">
      <ProviderListCard
        providers={providers}
        busy={provBusy}
        activeMissingKey={activeMissingKey}
        onActivate={(id) => void activateProvider(id)}
        onEdit={startEdit}
        onDelete={(p) => void deleteProvider(p)}
        onAdd={startNew}
      />
      <ProviderFormCard
        form={provForm}
        onFormChange={setProvForm}
        editingId={editingId}
        busy={provBusy}
        error={provError}
        revealKey={revealKey}
        onToggleRevealKey={() => setRevealKey((v) => !v)}
        test={llmTest}
        testBusy={llmBusy === "test"}
        actionsDisabled={actionsDisabled}
        onApplyPreset={applyPreset}
        onTest={() => void runLlmAction("test", true)}
        onSubmit={(e) => void saveProvider(e)}
        onCancel={startNew}
      />
      <GenerationCard
        form={llmForm}
        modelIds={modelIds}
        modelsError={llmModelsError}
        modelsBusy={llmBusy === "models"}
        actionsDisabled={actionsDisabled}
        selectedMax={selectedMax}
        onModelChange={selectModel}
        onModelsLoad={() => void runLlmAction("models", true)}
        onUpdate={updateLlmForm}
        onFocusField={focusLlmField}
        onBlurField={blurLlmField}
        onThinkingChange={setThinkingLevel}
      />
      <VisionCard
        form={llmForm}
        vision={vision}
        visionBusy={visionBusy}
        onVisionChange={setVisionChoice}
        onProbe={probeVision}
      />
      {error && <div className="set-error">{error}</div>}
    </div>
  );
}
