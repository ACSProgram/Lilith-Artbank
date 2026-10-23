import { createContext, useContext } from "react";
import type { AppInfo, LibraryInfo, Settings } from "./types";

export interface AppRuntime {
  settings: Settings | null;
  current: LibraryInfo | null;
  appInfo: AppInfo | null;
  loading: boolean;
  busy: string | null;
  addLibrary: (name: string, path: string) => Promise<LibraryInfo>;
  openLibrary: (libraryId: string) => Promise<LibraryInfo>;
  closeLibrary: () => Promise<void>;
  removeLibrary: (libraryId: string) => Promise<void>;
  renameLibrary: (libraryId: string, name: string) => Promise<void>;
  saveSettings: (next: Settings) => Promise<Settings>;
  refreshStatus: () => Promise<void>;
}

export const AppRuntimeContext = createContext<AppRuntime | null>(null);

export function useAppRuntime(): AppRuntime {
  const value = useContext(AppRuntimeContext);
  if (!value) throw new Error("useAppRuntime 必须在 AppRuntimeProvider 内使用");
  return value;
}
