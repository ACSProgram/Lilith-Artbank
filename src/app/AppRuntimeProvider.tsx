import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { appApi } from "./api";
import { useNotices } from "./noticeContext";
import { AppRuntimeContext, type AppRuntime } from "./runtimeContext";
import type { AppInfo, LibraryInfo, Settings } from "./types";
import { errorMessage } from "../shared/errors";

export function AppRuntimeProvider({ children }: { children: ReactNode }) {
  const { notify } = useNotices();
  const [settings, setSettings] = useState<Settings | null>(null);
  const [current, setCurrent] = useState<LibraryInfo | null>(null);
  const [appInfo, setAppInfo] = useState<AppInfo | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);

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
        const lastId = loadedSettings.lastLibraryId;
        if (lastId) {
          try {
            const opened = await appApi.openLibrary(lastId);
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

  const addLibrary = useCallback(
    async (name: string, path: string) => {
      const library = await run("正在打开图库", () => appApi.addLibraryPath(name, path));
      setCurrent(library);
      setSettings(await appApi.getSettings());
      notify(`已打开图库「${library.name}」`, "success");
      return library;
    },
    [notify, run],
  );

  const openLibrary = useCallback(
    async (libraryId: string) => {
      const library = await run("正在打开图库", () => appApi.openLibrary(libraryId));
      setCurrent(library);
      setSettings(await appApi.getSettings());
      return library;
    },
    [run],
  );

  const closeLibrary = useCallback(async () => {
    await run("正在关闭图库", () => appApi.closeLibrary());
    setCurrent(null);
  }, [run]);

  const removeLibrary = useCallback(
    async (libraryId: string) => {
      const updated = await run("正在移除图库", () => appApi.removeLibrary(libraryId));
      setSettings(updated);
      setCurrent((value) => (value?.id === libraryId ? null : value));
    },
    [run],
  );

  const renameLibrary = useCallback(
    async (libraryId: string, name: string) => {
      const updated = await run("正在重命名图库", () => appApi.renameLibrary(libraryId, name));
      setSettings(updated);
      setCurrent((value) => (value?.id === libraryId ? { ...value, name } : value));
    },
    [run],
  );

  const saveSettings = useCallback(
    async (next: Settings) => {
      const updated = await run("正在保存设置", () => appApi.saveSettings(next));
      setSettings(updated);
      notify("设置已保存", "success");
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
      addLibrary,
      openLibrary,
      closeLibrary,
      removeLibrary,
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
      addLibrary,
      openLibrary,
      closeLibrary,
      removeLibrary,
      renameLibrary,
      saveSettings,
      refreshStatus,
    ],
  );

  return <AppRuntimeContext.Provider value={value}>{children}</AppRuntimeContext.Provider>;
}
