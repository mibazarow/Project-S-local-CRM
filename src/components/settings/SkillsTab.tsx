import { useCallback, useEffect, useState, type FormEvent } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import * as ipc from "../../api/ipc";
import type { ImportResult, SkillInfo } from "../../api/types";
import { toMessage as errMsg } from "../../shared/errors";
import { Card, Field } from "./shared";
import { emptySkillForm, type SkillForm, type SkillKindOption } from "./forms";

export default function SkillsTab({ active }: { active: boolean }) {
  const [skills, setSkills] = useState<SkillInfo[] | null>(null);
  const [skillsError, setSkillsError] = useState<string | null>(null);
  const [skillsBusy, setSkillsBusy] = useState(false);
  const [overwrite, setOverwrite] = useState(false);
  const [gitUrl, setGitUrl] = useState("");
  const [importResults, setImportResults] = useState<ImportResult[] | null>(null);
  const [skillForm, setSkillForm] = useState<SkillForm>({ ...emptySkillForm });
  const [skillFormError, setSkillFormError] = useState<string | null>(null);
  const [skillNote, setSkillNote] = useState<string | null>(null);
  const [skillCreateBusy, setSkillCreateBusy] = useState(false);

  const loadSkills = useCallback(() => {
    void ipc
      .skillsList()
      .then(setSkills)
      .catch((e) => setSkillsError(errMsg(e)));
  }, []);

  useEffect(() => {
    loadSkills();
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void ipc
      .onSkillsChanged(() => loadSkills())
      .then((u) => (cancelled ? u() : (unlisten = u)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [loadSkills]);

  // skills-changed refreshes the list, so the toggle needs no manual reload.
  const toggleSkill = (name: string, enabled: boolean) => {
    setSkillsError(null);
    void ipc.skillsSetEnabled(name, enabled).catch((e) => setSkillsError(errMsg(e)));
  };

  const doImport = async (source: "folder" | "zip" | "git") => {
    setSkillsBusy(true);
    setSkillsError(null);
    setImportResults(null);
    try {
      let target = "";
      if (source === "git") {
        target = gitUrl.trim();
        if (!target) {
          setSkillsError("Укажи git-адрес репозитория со скиллами");
          return;
        }
      } else {
        const picked = await open(
          source === "folder"
            ? { directory: true, title: "Импорт скиллов из папки" }
            : {
                multiple: false,
                title: "Импорт скиллов из .zip",
                filters: [{ name: "Zip-архив", extensions: ["zip"] }],
              },
        );
        if (typeof picked !== "string" || !picked) return;
        target = picked;
      }
      setImportResults(await ipc.skillsImport(source, target, overwrite));
      if (source === "git") setGitUrl("");
    } catch (e) {
      setSkillsError(errMsg(e));
    } finally {
      setSkillsBusy(false);
    }
  };

  const installBuiltin = () => {
    setSkillsBusy(true);
    setSkillsError(null);
    setImportResults(null);
    void ipc
      .skillsInstallBuiltin()
      .then((names) => setImportResults(names.map((name) => ({ name, ok: true, error: null }))))
      .catch((e) => setSkillsError(errMsg(e)))
      .finally(() => setSkillsBusy(false));
  };

  // skills_create emits skills-changed, so the list refreshes on its own.
  const createSkill = (e: FormEvent) => {
    e.preventDefault();
    setSkillFormError(null);
    setSkillNote(null);
    const name = skillForm.name.trim();
    if (!/^[a-z0-9]([a-z0-9-]*[a-z0-9])?$/.test(name) || name.length > 64) {
      setSkillFormError("Имя: строчные латинские буквы, цифры и дефис, не длиннее 64 символов");
      return;
    }
    const description = skillForm.description.trim();
    if (!description) {
      setSkillFormError("Описание — одна строка, обязательно");
      return;
    }
    setSkillCreateBusy(true);
    void ipc
      .skillsCreate(
        name,
        description,
        skillForm.kind,
        skillForm.body.trim() || undefined,
        skillForm.overwrite,
      )
      .then(() => {
        setSkillForm({ ...emptySkillForm });
        setSkillNote(`Скилл «${name}» создан`);
      })
      .catch((err) => setSkillFormError(errMsg(err)))
      .finally(() => setSkillCreateBusy(false));
  };

  if (!active) return null;
  return (
    <div className="settings-content">
      <Card
        title="Импорт скиллов"
        desc="Загрузка скиллов из папки, архива или git-репозитория."
      >
        <Field
          label="Перезаписывать при импорте"
          hint="Заменяет уже установленный пользовательский скилл с тем же именем."
        >
          <label className="switch">
            <input
              type="checkbox"
              checked={overwrite}
              onChange={(e) => setOverwrite(e.target.checked)}
            />
            <span className="track"></span>
          </label>
        </Field>
        <Field
          label="Git-адрес репозитория со скиллами"
          wide
          hint="Нужен для кнопки «Импорт из git»; требуется системный git."
        >
          <input
            type="text"
            value={gitUrl}
            onChange={(e) => setGitUrl(e.target.value)}
            placeholder="https://github.com/example/skills"
            spellCheck={false}
          />
        </Field>
        <div className="set-actions">
          <button
            className="ghost-btn"
            disabled={skillsBusy}
            onClick={() => void doImport("folder")}
          >
            Импорт из папки
          </button>
          <button
            className="ghost-btn"
            disabled={skillsBusy}
            onClick={() => void doImport("zip")}
          >
            Импорт из .zip
          </button>
          <button
            className="ghost-btn"
            disabled={skillsBusy}
            onClick={() => void doImport("git")}
          >
            Импорт из git
          </button>
        </div>
        {importResults && (
          <div className="skill-results">
            {importResults.map((r, i) => (
              <div key={`${r.name}-${i}`} className={r.ok ? "set-note" : "set-error"}>
                {r.name}: {r.ok ? "импортирован" : r.error}
              </div>
            ))}
          </div>
        )}
      </Card>
      <Card
        title="Встроенные скиллы"
        desc="Копирует набор встроенных скиллов приложения в папку настроек."
      >
        <div className="set-actions">
          <button className="ghost-btn" disabled={skillsBusy} onClick={installBuiltin}>
            Установить встроенные
          </button>
        </div>
      </Card>
      <Card title="Создать скилл" desc="Новый пользовательский скилл появится в списке ниже.">
        <form className="set-form" onSubmit={createSkill}>
          <Field
            label="Имя (slug)"
            hint="Строчные латинские буквы, цифры и дефис; по этому имени агент и вызывает скилл."
          >
            <input
              type="text"
              value={skillForm.name}
              onChange={(e) => setSkillForm({ ...skillForm, name: e.target.value })}
              placeholder="my-skill"
              spellCheck={false}
            />
          </Field>
          <Field label="Описание (одна строка)">
            <input
              type="text"
              value={skillForm.description}
              onChange={(e) => setSkillForm({ ...skillForm, description: e.target.value })}
              placeholder="Что делает этот скилл"
            />
          </Field>
          <Field label="Тип">
            <select
              value={skillForm.kind}
              onChange={(e) =>
                setSkillForm({ ...skillForm, kind: e.target.value as SkillKindOption })
              }
            >
              <option value="instruction">instruction (инструкция)</option>
              <option value="script">script (скрипт)</option>
              <option value="extension">extension (расширение)</option>
            </select>
          </Field>
          <Field label="Тело скилла (markdown, необязательно)" wide>
            <textarea
              rows={6}
              value={skillForm.body}
              onChange={(e) => setSkillForm({ ...skillForm, body: e.target.value })}
              placeholder="Полные инструкции для агента…"
              spellCheck={false}
            />
          </Field>
          <Field label="перезаписать, если существует">
            <label className="switch">
              <input
                type="checkbox"
                checked={skillForm.overwrite}
                onChange={(e) =>
                  setSkillForm({ ...skillForm, overwrite: e.target.checked })
                }
              />
              <span className="track"></span>
            </label>
          </Field>
          <div className="set-actions">
            <button className="primary-btn small" type="submit" disabled={skillCreateBusy}>
              {skillCreateBusy ? "Создание…" : "Создать"}
            </button>
          </div>
          {skillFormError && <div className="set-error">{skillFormError}</div>}
          {skillNote && <div className="set-note">{skillNote}</div>}
        </form>
      </Card>
      <Card title="Список скиллов">
        {skills === null ? (
          <div className="set-empty">Загрузка…</div>
        ) : skills.length === 0 ? (
          <div className="set-empty">Скиллов нет. Установи встроенные или импортируй свои.</div>
        ) : (
          <div className="set-list">
            {skills.map((s, i) =>
              s.error ? (
                <div key={`${s.name}-${i}`} className="skill-item skill-item-error">
                  <div className="info">
                    <div className="t">{s.name || "(без имени)"}</div>
                    <div className="c">{s.error}</div>
                  </div>
                  {s.builtin && <span className="badge">встроенный</span>}
                </div>
              ) : (
                <div key={`${s.name}-${i}`} className="skill-item">
                  <div className="info">
                    <div className="t">{s.name}</div>
                    <div className="c">{s.description}</div>
                  </div>
                  {s.builtin && <span className="badge">встроенный</span>}
                  <label className="switch">
                    <input
                      type="checkbox"
                      checked={s.enabled}
                      onChange={(e) => toggleSkill(s.name, e.target.checked)}
                    />
                    <span className="track"></span>
                  </label>
                </div>
              ),
            )}
          </div>
        )}
      </Card>
      {skillsError && <div className="set-error">{skillsError}</div>}
    </div>
  );
}
