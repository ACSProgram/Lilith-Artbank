import { createContext, useContext } from "react";
import type { LibraryCountDelta } from "../modules/library/dataChange";
import type { AppInfo, LibraryInfo, Settings } from "./types";

export interface AppRuntime {
  settings: Settings | null;
  current: LibraryInfo | null;
  appInfo: AppInfo | null;
  loading: boolean;
  busy: string | null;
  fullscreen: boolean;
  setFullscreen: (value: boolean) => Promise<boolean>;
  toggleFullscreen: () => Promise<void>;
  createLibrary: (name: string, path: string) => Promise<LibraryInfo>;
  openLibrary: (path: string) => Promise<LibraryInfo>;
  closeLibrary: () => Promise<void>;
  renameLibrary: (name: string) => Promise<void>;
  saveSettings: (next: Settings, options?: { silent?: boolean }) => Promise<Settings>;
  refreshStatus: () => Promise<void>;
  /** 本地写入已知数量时直接改缓存计数，不再重查全库。 */
  adjustCounts: (delta: LibraryCountDelta) => void;
}

export const AppRuntimeContext = createContext<AppRuntime | null>(null);

export function useAppRuntime(): AppRuntime {
  const value = useContext(AppRuntimeContext);
  if (!value) throw new Error("useAppRuntime 必须在 AppRuntimeProvider 内使用");
  return value;
}
