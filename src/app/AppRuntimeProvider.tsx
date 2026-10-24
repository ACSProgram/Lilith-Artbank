import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { appApi, browserPreview } from "./api";
import { useNotices } from "./noticeContext";
import { AppRuntimeContext, type AppRuntime } from "./runtimeContext";
import { setThemeMode } from "./theme";
import { setThumbCacheLimit } from "../shared/thumbCache";
import type { AppInfo, LibraryInfo, Settings } from "./types";
import { errorMessage } from "../shared/errors";

export function AppRuntimeProvider({ children }: { children: ReactNode }) {
  const { notify } = useNotices();
  const [settings, setSettings] = useState<Settings | null>(null);
  const [current, setCurrent] = useState<LibraryInfo | null>(null);
  const [appInfo, setAppInfo] = useState<AppInfo | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [fullscreen, setFullscreenState] = useState(false);
  const fullscreenRef = useRef(false);
  fullscreenRef.current = fullscreen;

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [loadedSettings, status, info] = await Promise.all([
          appApi.getSettings(),
          appApi.getLibraryStatus(),
          appApi.getAppInfo(),
        ]);
        if (cancelled) return;
        setSettings(loadedSettings);
        setAppInfo(info);
        if (status) {
          setCurrent(status);
          return;
        }
        if (loadedSettings.libraryPath) {
          try {
            const opened = await appApi.openLibraryPath(loadedSettings.libraryPath);
            if (!cancelled) setCurrent(opened);
          } catch (error) {
            if (!cancelled) {
              notify("上次打开的图库无法加载", "error", errorMessage(error));
            }
          }
        }
      } catch (error) {
        if (!cancelled) notify("应用初始化失败", "error", errorMessage(error));
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [notify]);

  // 设置里的主题真正生效：加载完成、以及每次保存设置后都重新应用。
  useEffect(() => {
    setThemeMode(settings?.ui.theme ?? "system");
  }, [settings?.ui.theme]);

  // 缩略图缓存按张数计，改设置后立即收缩到新上限。
  useEffect(() => {
    setThumbCacheLimit(settings?.ui.thumbCacheCount ?? 1024);
  }, [settings?.ui.thumbCacheCount]);

  // 全屏状态由窗口真实状态决定：系统快捷键（F11、Win+↑）改变时按钮图标也能同步。
  useEffect(() => {
    if (browserPreview) {
      const sync = () => setFullscreenState(Boolean(document.fullscreenElement));
      document.addEventListener("fullscreenchange", sync);
      sync();
      return () => document.removeEventListener("fullscreenchange", sync);
    }
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void (async () => {
      const { getCurrentWindow } = await import("@tauri-apps/api/window");
      if (disposed) return;
      const window = getCurrentWindow();
      const sync = () => {
        void window
          .isFullscreen()
          .then((value) => {
            if (!disposed) setFullscreenState(value);
          })
          .catch(() => undefined);
      };
      sync();
      const stop = await window.onResized(sync);
      if (disposed) stop();
      else unlisten = stop;
    })();
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const setFullscreen = useCallback(
    async (value: boolean) => {
      try {
        const actual = await appApi.windowSetFullscreen(value);
        setFullscreenState(actual);
        return actual;
      } catch (cause) {
        notify("切换全屏失败", "error", errorMessage(cause));
        return fullscreenRef.current;
      }
    },
    [notify],
  );

  const toggleFullscreen = useCallback(async () => {
    await setFullscreen(!fullscreenRef.current);
  }, [setFullscreen]);

  const run = useCallback(
    async <T,>(label: string, operation: () => Promise<T>): Promise<T> => {
      setBusy(label);
      try {
        return await operation();
      } finally {
        setBusy(null);
      }
    },
    [],
  );

  const createLibrary = useCallback(
    async (name: string, path: string) => {
      const library = await run("正在创建图库", () => appApi.createLibrary(name, path));
      setCurrent(library);
      setSettings(await appApi.getSettings());
      notify(`已创建图库「${library.name}」`, "success");
      return library;
    },
    [notify, run],
  );

  const openLibrary = useCallback(
    async (path: string) => {
      const library = await run("正在打开图库", () => appApi.openLibraryPath(path));
      setCurrent(library);
      setSettings(await appApi.getSettings());
      notify(`已打开图库「${library.name}」`, "success");
      return library;
    },
    [notify, run],
  );

  const closeLibrary = useCallback(async () => {
    await run("正在关闭图库", () => appApi.closeLibrary());
    setCurrent(null);
    setSettings(await appApi.getSettings());
  }, [run]);

  const renameLibrary = useCallback(
    async (name: string) => {
      const renamed = await run("正在重命名图库", () => appApi.renameLibrary(name));
      setCurrent(renamed);
    },
    [run],
  );

  const saveSettings = useCallback(
    async (next: Settings, options?: { silent?: boolean }) => {
      const updated = await run("正在保存设置", () => appApi.saveSettings(next));
      setSettings(updated);
      if (!options?.silent) notify("设置已保存", "success");
      return updated;
    },
    [notify, run],
  );

  const refreshStatus = useCallback(async () => {
    const status = await appApi.getLibraryStatus();
    setCurrent(status);
  }, []);

  const value = useMemo<AppRuntime>(
    () => ({
      settings,
      current,
      appInfo,
      loading,
      busy,
      fullscreen,
      setFullscreen,
      toggleFullscreen,
      createLibrary,
      openLibrary,
      closeLibrary,
      renameLibrary,
      saveSettings,
      refreshStatus,
    }),
    [
      settings,
      current,
      appInfo,
      loading,
      busy,
      fullscreen,
      setFullscreen,
      toggleFullscreen,
      createLibrary,
      openLibrary,
      closeLibrary,
      renameLibrary,
      saveSettings,
      refreshStatus,
    ],
  );

  return <AppRuntimeContext.Provider value={value}>{children}</AppRuntimeContext.Provider>;
}
