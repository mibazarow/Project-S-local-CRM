import { useEffect, useState } from "react";
import * as ipc from "../../api/ipc";
import type { EmbedStatus, IndexingConfig } from "../../api/types";
import { toMessage as errMsg } from "../../shared/errors";
import { Card, Field } from "./shared";
import { useDebouncedConfig, type DebouncedConfig } from "./useDebouncedConfig";
import { indexingToPatch, toForm, type AppForm, type Form } from "./forms";

export default function AgentTab({
  active,
  error,
  setError,
  app,
}: {
  active: boolean;
  error: string | null;
  setError: (message: string) => void;
  app: DebouncedConfig<AppForm>;
}) {
  const { form, updateForm, focusField, blurField, reload } = useDebouncedConfig<Form>(
    "indexing",
    (raw) => toForm(raw as unknown as IndexingConfig),
    indexingToPatch,
    setError,
  );
  const [embed, setEmbed] = useState<EmbedStatus | null>(null);
  const [reindexBusy, setReindexBusy] = useState(false);
  const [reindexNote, setReindexNote] = useState<string | null>(null);

  useEffect(() => {
    void ipc
      .embedStatus()
      .then(setEmbed)
      .catch(() => setEmbed(null));
  }, []);

  useEffect(() => {
    let cancelled = false;
    const unlisteners: (() => void)[] = [];
    const track = (p: Promise<() => void>) =>
      p.then((u) => (cancelled ? u() : unlisteners.push(u)));
    track(
      ipc.onConfigChanged(({ which }) => {
        if (which === "indexing") reload();
      }),
    );
    track(
      ipc.onIndexChanged(({ stats }) =>
        setReindexNote(
          `Индекс обновлён: файлов ${stats.filesIndexed}, убрано ${stats.filesRemoved}`,
        ),
      ),
    );
    return () => {
      cancelled = true;
      unlisteners.forEach((u) => u());
    };
  }, [reload]);

  const reindex = () => {
    setReindexBusy(true);
    setReindexNote("Переиндексация…");
    void ipc
      .reindexAll()
      .then((results) => {
        const indexed = results.reduce((s, r) => s + r.stats.filesIndexed, 0);
        const seen = results.reduce((s, r) => s + r.stats.filesSeen, 0);
        const errors = results.reduce((s, r) => s + r.stats.errors.length, 0);
        setReindexNote(
          `Готово: индексировано ${indexed} из ${seen} файлов${errors ? `, ошибок: ${errors}` : ""}`,
        );
      })
      .catch((e) => setError(errMsg(e)))
      .finally(() => setReindexBusy(false));
  };

  if (!active) return null;
  return (
    <div className="settings-content">
      <Card title="Автопилот">
        <Field
          label="Полный автопилот (все разрешения без подтверждений)"
          hint="Опасно: агент выполняет любые действия без подтверждений. По умолчанию выключено."
        >
          <label className="switch">
            <input
              type="checkbox"
              checked={app.form?.autopilot ?? false}
              onChange={(e) => app.updateForm({ autopilot: e.target.checked })}
            />
            <span className="track"></span>
          </label>
        </Field>
      </Card>
      <Card title="Индексация" desc="Ограничения на файлы, попадающие в поиск по проектам.">
        <Field
          label="Максимальный размер файла, МБ"
          hint="Файлы больше этого размера в индекс не попадают. По умолчанию 10 МБ."
        >
          <input
            className="narrow"
            type="text"
            inputMode="decimal"
            value={form?.maxMb ?? ""}
            onChange={(e) => updateForm({ maxMb: e.target.value })}
            onFocus={focusField("maxMb")}
            onBlur={blurField}
          />
        </Field>
        <Field
          label="Лимит извлечённого текста, символов"
          hint="Из одного большого файла берётся не больше этого куска текста. По умолчанию 1 000 000 символов."
        >
          <input
            className="narrow"
            type="text"
            inputMode="numeric"
            value={form?.maxChars ?? ""}
            onChange={(e) => updateForm({ maxChars: e.target.value })}
            onFocus={focusField("maxChars")}
            onBlur={blurField}
          />
        </Field>
        <Field
          label="Исключать из индекса (через запятую)"
          wide
          hint="Папки, которые поиск по проекту никогда не увидит."
        >
          <input
            type="text"
            value={form?.excludeDirs ?? ""}
            onChange={(e) => updateForm({ excludeDirs: e.target.value })}
            onFocus={focusField("excludeDirs")}
            onBlur={blurField}
          />
        </Field>
      </Card>
      <Card title="Переиндексация" desc="Полное перестроение индекса по всем проектам.">
        <div className="set-actions">
          <button className="ghost-btn" disabled={reindexBusy} onClick={reindex}>
            {reindexBusy ? "Переиндексация…" : "Переиндексировать"}
          </button>
        </div>
        {reindexNote && <div className="set-note">{reindexNote}</div>}
      </Card>
      <Card
        title="Семантический поиск"
        desc="Семантический поиск включён всегда: поиск по словам и по смыслу работают вместе (локальная модель EmbeddingGemma 2, векторы 768). Отключить или подстроить его нельзя."
      >
        {embed && (
          <div className="set-note">
            Статус модели:{" "}
            {embed.available
              ? "скачана, активна"
              : embed.modelPresent
                ? "файлы на месте, но модель недоступна"
                : "не скачана — сейчас поиск только по словам"}
          </div>
        )}
        {embed?.reason && <div className="set-note">{embed.reason}</div>}
      </Card>
      {error && <div className="set-error">{error}</div>}
    </div>
  );
}
