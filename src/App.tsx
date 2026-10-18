import { useEffect, useState } from "react";
import { useApp } from "./state/AppStore";
import Onboarding from "./components/onboarding/Onboarding";
import TopBar, { type Tab } from "./components/TopBar";
import CommandPalette from "./components/CommandPalette";
import SettingsModal from "./components/SettingsModal";
import TodayView from "./components/today/TodayView";
import ProjectList from "./components/projects/ProjectList";
import FilesPanel from "./components/files/FilesPanel";
import ProjectChat from "./components/chat/ProjectChat";
import HarnessView from "./components/harness/HarnessView";
import NotesView from "./components/notes/NotesView";

export default function App() {
  const { state, actions } = useApp();
  const [tab, setTab] = useState<Tab>("projects");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.code === "KeyK") {
        e.preventDefault();
        setPaletteOpen((v) => !v);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  useEffect(() => {
    if (!state.settingsRequested) return;
    setSettingsOpen(true);
    actions.clearSettingsRequested();
  }, [state.settingsRequested, actions]);

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
      <TopBar tab={tab} onTab={setTab} onSettings={() => setSettingsOpen(true)} onSearch={() => setPaletteOpen(true)} />

      {tab === "today" ? (
        <TodayView onTab={setTab} />
      ) : tab === "projects" ? (
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
      ) : tab === "harness" ? (
        <HarnessView />
      ) : (
        <NotesView />
      )}

      {settingsOpen && <SettingsModal onClose={() => setSettingsOpen(false)} />}
      {paletteOpen && <CommandPalette onTab={setTab} onClose={() => setPaletteOpen(false)} />}
    </div>
  );
}
