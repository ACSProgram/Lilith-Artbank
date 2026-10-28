import { useEffect, useState } from "react";
import brandMark from "../assets/brand-mark.png";
import { useAppRuntime } from "./runtimeContext";
import { LibraryDialog } from "./LibraryDialog";
import { LibraryWorkspace } from "./LibraryWorkspace";
import { SettingsDialog } from "./SettingsDialog";
import { ShortcutSheet } from "./ShortcutSheet";
import { Sidebar } from "./Sidebar";
import { WelcomeView } from "./WelcomeView";
import { WindowTitleBar } from "./WindowTitleBar";
import { Spinner } from "../shared/components/Spinner";

/** 输入框 / 文本域 / 下拉 / 可编辑区里不抢键位。 */
function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  return ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName);
}

export function AppShell() {
  const { loading, current, closeLibrary, fullscreen, toggleFullscreen } = useAppRuntime();
  const [libraryDialogOpen, setLibraryDialogOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [shortcutsOpen, setShortcutsOpen] = useState(false);

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
        return;
      }
      if (key === "f11") {
        event.preventDefault();
        void toggleFullscreen();
        return;
      }
      // `?` 是 shift + /；只在不在输入框里时打开快捷键总表。
      if (event.key === "?" && !isTypingTarget(event.target)) {
        event.preventDefault();
        setShortcutsOpen(true);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [toggleFullscreen]);

  if (loading) {
    return (
      <div className="app-frame">
        <WindowTitleBar />
        <div className="splash">
          <img className="splash-mark" src={brandMark} alt="" draggable={false} />
          <Spinner size={22} />
          <p>正在读取设置与图库状态…</p>
        </div>
      </div>
    );
  }

  return (
    <div className={`app-frame${fullscreen ? " is-fullscreen" : ""}`}>
      {fullscreen ? null : <WindowTitleBar />}
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
        onOpenShortcuts={() => setShortcutsOpen(true)}
        onChangeLibrary={() => {
          setSettingsOpen(false);
          setLibraryDialogOpen(true);
        }}
        onCloseLibrary={closeLibrary}
      />
      <ShortcutSheet open={shortcutsOpen} onClose={() => setShortcutsOpen(false)} />
    </div>
  );
}
