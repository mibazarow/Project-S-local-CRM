import { useState } from "react";
import { useApp } from "./state/AppStore";
import Onboarding from "./components/onboarding/Onboarding";
import TopBar, { type Tab } from "./components/TopBar";
import SettingsModal from "./components/SettingsModal";
import ProjectList from "./components/projects/ProjectList";
import FilesPanel from "./components/files/FilesPanel";
import ProjectChat from "./components/chat/ProjectChat";
import HarnessView from "./components/harness/HarnessView";

export default function App() {
  const { state } = useApp();
  const [tab, setTab] = useState<Tab>("projects");
  const [settingsOpen, setSettingsOpen] = useState(false);

  if (state.loading) {
    return (
      <div className="app">
        <div className="harness-body">
          <div className="placeholder">
            <h1>Project S</h1>
            <p>Загрузка…</p>
          </div>
        </div>
      </div>
    );
  }

  if (!state.onboarding.configured) return <Onboarding />;

  const activeProject = state.projects.find((p) => p.id === state.activeProjectId) ?? null;

  return (
    <div className="app">
      <TopBar tab={tab} onTab={setTab} onSettings={() => setSettingsOpen(true)} />

      {tab === "projects" ? (
        <main className="view active">
          <ProjectList />

          {activeProject ? (
            <FilesPanel key={activeProject.id} project={activeProject} />
          ) : (
            <section className="center">
              <div className="center-body">
                <div className="placeholder">
                  <h1>Проекты</h1>
                  <p>Выберите проект слева или создайте новый</p>
                </div>
              </div>
            </section>
          )}

          <ProjectChat projectId={activeProject?.id ?? null} projectName={activeProject?.name ?? null} />
        </main>
      ) : (
        <HarnessView />
      )}

      {settingsOpen && <SettingsModal onClose={() => setSettingsOpen(false)} />}
    </div>
  );
}
