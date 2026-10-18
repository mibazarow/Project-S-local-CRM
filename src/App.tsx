import { useEffect, useState } from "react";
import * as ipc from "./api/ipc";
import { useApp } from "./state/AppStore";
import Onboarding from "./components/onboarding/Onboarding";
import TopBar, { type Tab } from "./components/TopBar";
import CommandPalette from "./components/CommandPalette";
import QuickCapture from "./components/QuickCapture";
import SettingsModal from "./components/SettingsModal";
import TodayView from "./components/today/TodayView";
import ProjectList from "./components/projects/ProjectList";
import FilesPanel from "./components/files/FilesPanel";
import ProjectChat from "./components/chat/ProjectChat";
import HarnessView from "./components/harness/HarnessView";
import NotesView from "./components/notes/NotesView";
import Toasts from "./components/shared/toast";
import { setNoteLinkHandler } from "./shared/noteLink";

export default function App() {
  const { state, actions } = useApp();
  const [tab, setTab] = useState<Tab>("projects");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [captureTarget, setCaptureTarget] = useState<"note" | "task" | null>(null);

  // Quick capture entry points: the global shortcut opens note mode; the
  // tray menu routes «Быстрая заметка» / «Новая задача» to the matching mode.
  useEffect(() => {
    let cancelled = false;
    let unQuick: (() => void) | undefined;
    let unTray: (() => void) | undefined;
    void ipc
      .onQuickCapture(() => setCaptureTarget("note"))
      .then((u) => (cancelled ? u() : (unQuick = u)));
    void ipc
      .onTrayAction(({ action }) => {
        if (action === "quick-note") setCaptureTarget("note");
        else if (action === "new-task") setCaptureTarget("task");
      })
      .then((u) => (cancelled ? u() : (unTray = u)));
    return () => {
      cancelled = true;
      unQuick?.();
      unTray?.();
    };
  }, []);

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

  // Cross-entity quick actions target the project chat: bring the projects
  // view on screen while ProjectChat consumes the pending question.
  useEffect(() => {
    if (state.pendingAsk) setTab("projects");
  }, [state.pendingAsk]);

  // A note link clicked in any agent answer opens that note inside the Notes
  // section (not externally): reuse the pendingNote intent and switch tabs.
  useEffect(() => {
    setNoteLinkHandler((relPath) => {
      actions.requestNote(relPath);
      setTab("notes");
    });
    return () => setNoteLinkHandler(null);
  }, [actions]);

  if (state.loading) {
    return (
      <div className="app">
        <div className="harness-body">
          <div className="placeholder">
            <h1>Project-S</h1>
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
      {captureTarget && (
        <QuickCapture initialTarget={captureTarget} onClose={() => setCaptureTarget(null)} />
      )}
      <Toasts />
    </div>
  );
}
