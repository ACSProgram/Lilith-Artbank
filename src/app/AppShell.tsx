import { useEffect, useState } from "react";
import { useAppRuntime } from "./runtimeContext";
import { LibraryDialog } from "./LibraryDialog";
import { LibraryWorkspace } from "./LibraryWorkspace";
import { SettingsDialog } from "./SettingsDialog";
import { Sidebar } from "./Sidebar";
import { WelcomeView } from "./WelcomeView";
import { WindowTitleBar } from "./WindowTitleBar";
import { Spinner } from "../shared/components/Spinner";

export function AppShell() {
  const { loading, current, closeLibrary } = useAppRuntime();
  const [libraryDialogOpen, setLibraryDialogOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.defaultPrevented) return;
      const key = event.key.toLowerCase();
      if (key === "f5" || (event.ctrlKey && key === "r")) {
        event.preventDefault();
        return;
      }
      if (event.ctrlKey && key === ",") {
        event.preventDefault();
        setSettingsOpen(true);
        return;
      }
      if (event.ctrlKey && key === "o") {
        event.preventDefault();
        setLibraryDialogOpen(true);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  if (loading) {
    return (
      <div className="app-frame">
        <WindowTitleBar />
        <div className="splash">
          <div className="splash-mark">A</div>
          <Spinner size={22} />
          <p>正在读取设置与图库状态…</p>
        </div>
      </div>
    );
  }

  return (
    <div className="app-frame">
      <WindowTitleBar />
      {current ? (
        <LibraryWorkspace
          key={current.id}
          onOpenSettings={() => setSettingsOpen(true)}
          onChangeLibrary={() => setLibraryDialogOpen(true)}
        />
      ) : (
        <div className="app-body">
          <Sidebar
            onOpenSettings={() => setSettingsOpen(true)}
            onOpenLibrary={() => setLibraryDialogOpen(true)}
          />
          <main className="app-main">
            <WelcomeView onAddLibrary={() => setLibraryDialogOpen(true)} />
          </main>
        </div>
      )}
      <LibraryDialog open={libraryDialogOpen} onClose={() => setLibraryDialogOpen(false)} />
      <SettingsDialog
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
        onChangeLibrary={() => {
          setSettingsOpen(false);
          setLibraryDialogOpen(true);
        }}
        onCloseLibrary={closeLibrary}
      />
    </div>
  );
}
