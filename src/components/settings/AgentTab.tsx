import { useEffect, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import type { EmbedStatus, IndexingConfig, ProjectIndexStats } from "../../api/types";
import { toMessage as errMsg } from "../../shared/errors";
import { confirmAction } from "../../shared/confirm";
import { startEmbedModelDownload, useEmbedModelDownload } from "../../state/embedModelDownload";
import { Card, Field } from "./shared";
import Switch from "../shared/Switch";
import { useDebouncedConfig, type DebouncedConfig } from "./useDebouncedConfig";
import { indexingToPatch, toForm, type AppForm, type Form } from "./forms";

const MAX_SHOWN_ERRORS = 3;

function summarize(results: ProjectIndexStats[]): string {
  const seen = results.reduce((s, r) => s + r.stats.filesSeen, 0);
  const updated = results.reduce((s, r) => s + r.stats.filesIndexed, 0);
  const unchanged = results.reduce((s, r) => s + r.stats.filesSkipped, 0);
  const removed = results.reduce((s, r) => s + r.stats.filesRemoved, 0);
  const errors = results.flatMap((r) => r.stats.errors);
  const counters =
    updated === 0 && removed === 0
      ? "Индекс актуален"
      : `Проверено ${seen}, обновлено ${updated}, без изменений ${unchanged}, удалено ${removed}`;
  if (errors.length === 0) return counters;
  const rest = errors.length > MAX_SHOWN_ERRORS ? ` (+${errors.length - MAX_SHOWN_ERRORS})` : "";
  return `${counters}. Ошибки: ${errors.slice(0, MAX_SHOWN_ERRORS).join("; ")}${rest}`;
}

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
  const dl = useEmbedModelDownload();
  const [reindexBusy, setReindexBusy] = useState<"inc" | "hard" | null>(null);
  const [reindexNote, setReindexNote] = useState<string | null>(null);
  const [styleBusy, setStyleBusy] = useState(false);
  const [styleNote, setStyleNote] = useState<string | null>(null);
  const reindexBusyRef = useRef(false);
  reindexBusyRef.current = reindexBusy !== null;

  useEffect(() => {
    void ipc
      .embedStatus()
      .then(setEmbed)
      .catch(() => setEmbed(null));
  }, []);

  // A finished model download changes the embed status: re-read it so the
  // card switches to "скачана, активна" without reopening settings.
  useEffect(() => {
    if (dl.phase !== "done") return;
    void ipc
      .embedStatus()
      .then(setEmbed)
      .catch(() => {});
  }, [dl.phase]);

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
      ipc.onIndexChanged(({ projectId, stats }) => {
        if (!reindexBusyRef.current) {
          setReindexNote(summarize([{ projectId, stats }]));
        }
      }),
    );
    return () => {
      cancelled = true;
      unlisteners.forEach((u) => u());
    };
  }, [reload]);

  const reindex = () => {
    setReindexBusy("inc");
    setReindexNote("Переиндексация…");
    void ipc
      .reindexAll()
      .then((results) => setReindexNote(summarize(results)))
      .catch((e) => setError(errMsg(e)))
      .finally(() => setReindexBusy(null));
  };

  const reindexHard = async () => {
    const ok = await confirmAction({
      title: "Пересобрать индекс полностью?",
      message:
        "Весь текущий индекс файлов (поиск по словам и векторы) будет очищен и построен заново по всем проектам и заметкам. Это лечит испорченный индекс; сборка может занять время.",
      confirmLabel: "Пересобрать",
      danger: true,
    });
    if (!ok) return;
    setReindexBusy("hard");
    setReindexNote("Пересборка индекса…");
    void ipc
      .reindexHard()
      .then((results) => setReindexNote(summarize(results)))
      .catch((e) => setError(errMsg(e)))
      .finally(() => setReindexBusy(null));
  };

  const resetStyle = () => {
    setStyleBusy(true);
    void ipc
      .styleReset()
      .then(() => setStyleNote("Стиль общения сброшен: агент начнёт с нейтрального тона."))
      .catch((e) => setError(errMsg(e)))
      .finally(() => setStyleBusy(false));
  };

  if (!active) return null;
  return (
    <div className="settings-content">
      <Card title="Автопилот">
        <Field
          label="Полный автопилот (все разрешения без подтверждений)"
          hint="Опасно: агент выполняет любые действия без подтверждений. По умолчанию выключено."
        >
          <Switch
            checked={app.form?.autopilot ?? false}
            onChange={(e) => app.updateForm({ autopilot: e.target.checked })}
          />
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
      <Card title="Переиндексация" desc="Обход файлов по всем проектам: новые и изменённые обновляются, удалённые убираются из поиска.">
        <div className="set-actions">
          <button className="ghost-btn" disabled={reindexBusy !== null} onClick={reindex}>
            {reindexBusy === "inc" ? "Переиндексация…" : "Переиндексировать"}
          </button>
          <button className="ghost-btn" disabled={reindexBusy !== null} onClick={reindexHard}>
            {reindexBusy === "hard" ? "Пересборка…" : "Пересобрать индекс полностью"}
          </button>
        </div>
        {reindexNote && <div className="set-note">{reindexNote}</div>}
      </Card>
      <Card
        title="Стиль общения"
        desc="Агент постепенно перенимает тон, обращение (ты/вы) и манеру пользователя во всех чатах. Сброс возвращает нейтральный стиль."
      >
        <div className="set-actions">
          <button className="ghost-btn" disabled={styleBusy} onClick={resetStyle}>
            {styleBusy ? "Сброс…" : "Сбросить стиль общения"}
          </button>
        </div>
        {styleNote && <div className="set-note">{styleNote}</div>}
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
                ? "файлы на месте, но модель недоступна — векторы не строятся, поиск только по словам"
                : "не скачана — векторы не строятся, поиск только по словам"}
          </div>
        )}
        {embed?.reason && <div className="set-note">{embed.reason}</div>}
        {embed && !embed.modelPresent && (
          <>
            <div className="set-actions">
              <button
                className="ghost-btn"
                disabled={dl.phase === "checking" || dl.phase === "downloading"}
                onClick={() => void startEmbedModelDownload()}
              >
                {dl.phase === "checking"
                  ? "Проверяем…"
                  : dl.phase === "downloading"
                    ? "Скачиваем модель…"
                    : dl.phase === "error"
                      ? "Повторить"
                      : "Скачать модель (~300 МБ)"}
              </button>
            </div>
            {dl.phase === "downloading" && (
              <div className="set-note">
                {dl.total > 0 ? `${Math.min(100, Math.round((dl.done / dl.total) * 100))}%` : "…"}
                {dl.file && ` · ${dl.file}`} · всего ~300 МБ
              </div>
            )}
            {dl.phase === "error" && dl.error && <div className="set-note">{dl.error}</div>}
          </>
        )}
      </Card>
      {error && <div className="set-error">{error}</div>}
    </div>
  );
}
