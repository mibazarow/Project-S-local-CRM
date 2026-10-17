import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { defaultWorkingDir, getAutostart, setAutostart } from "../../api/ipc";
import { useApp } from "../../state/AppStore";

export default function Onboarding() {
  const { state, actions } = useApp();
  const [defaultPath, setDefaultPath] = useState("");
  const [busy, setBusy] = useState(false);
  const [autostart, setAutostartState] = useState(false);
  const [autostartNote, setAutostartNote] = useState("");

  useEffect(() => {
    void defaultWorkingDir().then(setDefaultPath).catch(() => setDefaultPath(""));
    getAutostart().then(setAutostartState).catch(() => setAutostartState(false));
  }, []);

  const run = async (path: string) => {
    setBusy(true);
    try {
      await actions.chooseWorkingDir(path);
      try {
        await setAutostart(autostart);
      } catch (e) {
        console.warn("set_autostart failed:", e);
        setAutostartNote("Не удалось применить запуск при входе. Можно включить позже в Настройки → Прочее.");
      }
    } finally {
      setBusy(false);
    }
  };

  const chooseCustom = async () => {
    const path = await open({ directory: true, title: "Выбрать рабочую папку" });
    if (typeof path === "string" && path) await run(path);
  };

  return (
    <div className="onboarding">
      <div className="onboarding-card">
        <h1>Добро пожаловать в Project S</h1>
        <p>
          Выбери папку, где приложение будет хранить всё своё. В ней будут созданы три папки:
          <span className="onboarding-tree">Projects/</span>
          <span className="onboarding-tree">Harness/</span>
          <span className="onboarding-tree">Settings/</span>
        </p>
        <p className="onboarding-note">
          Всё лежит открыто в этих папках: проекты, настройки и база. Ничего не прячется — при
          желании можно перенести папку на другую машину.
        </p>

        {defaultPath && (
          <div className="onboarding-path">
            <span className="onboarding-path-label">Папка по умолчанию</span>
            <code>{defaultPath}</code>
          </div>
        )}

        <div className="set-row switch-row">
          <label>Запускать Project S при входе в систему</label>
          <label className="switch">
            <input
              type="checkbox"
              checked={autostart}
              disabled={busy}
              onChange={(e) => setAutostartState(e.target.checked)}
            />
            <span className="track" />
          </label>
        </div>
        <div className="set-hint">
          Если приложение не запущено, фоновые задачи и запуски по расписанию не выполняются.
          Позже это можно изменить в Настройки → Прочее.
        </div>
        {autostartNote && <div className="set-hint">{autostartNote}</div>}

        {state.error && <div className="onboarding-error">{state.error}</div>}

        <div className="onboarding-actions">
          <button
            className="primary-btn"
            disabled={busy || !defaultPath}
            onClick={() => void run(defaultPath)}
          >
            Использовать по умолчанию
          </button>
          <button className="ghost-btn" disabled={busy} onClick={() => void chooseCustom()}>
            Выбрать папку…
          </button>
        </div>
      </div>
    </div>
  );
}
