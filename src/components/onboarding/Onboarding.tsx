import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { defaultWorkingDir } from "../../api/ipc";
import { useApp } from "../../state/AppStore";

export default function Onboarding() {
  const { state, actions } = useApp();
  const [defaultPath, setDefaultPath] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    void defaultWorkingDir().then(setDefaultPath).catch(() => setDefaultPath(""));
  }, []);

  const run = async (path: string) => {
    setBusy(true);
    try {
      await actions.chooseWorkingDir(path);
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
