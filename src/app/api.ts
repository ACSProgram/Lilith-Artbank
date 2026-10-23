import { invoke } from "@tauri-apps/api/core";
import type {
  AppInfo,
  LibraryEntry,
  LibraryInfo,
  LibraryInspection,
  Settings,
} from "./types";

const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

const defaultSettings: Settings = {
  settingsVersion: 1,
  libraries: [],
  lastLibraryId: null,
  ai: {
    baseUrl: "http://127.0.0.1:18766",
    executablePath: null,
    port: 18766,
    autoStart: true,
  },
  performance: {
    readerConnections: 4,
    workerThreads: null,
    thumbnailCacheMb: 1024,
  },
  ui: {
    theme: "dark",
    gridThumbPx: 220,
  },
};

let mockSettings: Settings = structuredClone(defaultSettings);
let mockCurrent: LibraryInfo | null = null;

function mockLibrary(name: string, path: string): LibraryInfo {
  return {
    id: crypto.randomUUID(),
    name,
    path,
    imageCount: 0,
    trashCount: 0,
    schemaVersion: 1,
    openedAt: Date.now(),
  };
}

function mockEntry(library: LibraryInfo): LibraryEntry {
  return {
    id: library.id,
    name: library.name,
    path: library.path,
    addedAt: Date.now(),
  };
}

export const appApi = {
  async getSettings(): Promise<Settings> {
    if (!isTauri()) return structuredClone(mockSettings);
    return invoke<Settings>("get_settings");
  },

  async saveSettings(settings: Settings): Promise<Settings> {
    if (!isTauri()) {
      mockSettings = structuredClone(settings);
      return structuredClone(mockSettings);
    }
    return invoke<Settings>("save_settings", { settings });
  },

  async inspectLibraryPath(path: string): Promise<LibraryInspection> {
    if (!isTauri()) {
      return path.trim()
        ? { kind: "empty" }
        : { kind: "invalid", reason: "路径不能为空" };
    }
    return invoke<LibraryInspection>("inspect_library_path", { path });
  },

  async createLibrary(name: string, path: string): Promise<LibraryInfo> {
    if (!isTauri()) {
      const library = mockLibrary(name || "新图库", path);
      mockCurrent = library;
      mockSettings = {
        ...mockSettings,
        libraries: [...mockSettings.libraries, mockEntry(library)],
        lastLibraryId: library.id,
      };
      return library;
    }
    return invoke<LibraryInfo>("create_library", { name, path });
  },

  async addLibraryPath(name: string, path: string): Promise<LibraryInfo> {
    if (!isTauri()) {
      const library = mockLibrary(name || "新图库", path);
      mockCurrent = library;
      mockSettings = {
        ...mockSettings,
        libraries: [...mockSettings.libraries, mockEntry(library)],
        lastLibraryId: library.id,
      };
      return library;
    }
    return invoke<LibraryInfo>("add_library_path", { name, path });
  },

  async openLibrary(libraryId: string): Promise<LibraryInfo> {
    if (!isTauri()) {
      const entry = mockSettings.libraries.find((library) => library.id === libraryId);
      if (!entry) throw new Error("图库不在列表中");
      mockCurrent = { ...entry, imageCount: 0, trashCount: 0, schemaVersion: 1, openedAt: Date.now() };
      return mockCurrent;
    }
    return invoke<LibraryInfo>("open_library", { libraryId });
  },

  async closeLibrary(): Promise<void> {
    if (!isTauri()) {
      mockCurrent = null;
      return;
    }
    return invoke<void>("close_library");
  },

  async getLibraryStatus(): Promise<LibraryInfo | null> {
    if (!isTauri()) return mockCurrent;
    return invoke<LibraryInfo | null>("get_library_status");
  },

  async removeLibrary(libraryId: string): Promise<Settings> {
    if (!isTauri()) {
      mockSettings = {
        ...mockSettings,
        libraries: mockSettings.libraries.filter((library) => library.id !== libraryId),
        lastLibraryId: mockSettings.lastLibraryId === libraryId ? null : mockSettings.lastLibraryId,
      };
      if (mockCurrent?.id === libraryId) mockCurrent = null;
      return structuredClone(mockSettings);
    }
    return invoke<Settings>("remove_library", { libraryId });
  },

  async renameLibrary(libraryId: string, name: string): Promise<Settings> {
    if (!isTauri()) {
      mockSettings = {
        ...mockSettings,
        libraries: mockSettings.libraries.map((library) =>
          library.id === libraryId ? { ...library, name } : library,
        ),
      };
      if (mockCurrent?.id === libraryId) mockCurrent = { ...mockCurrent, name };
      return structuredClone(mockSettings);
    }
    return invoke<Settings>("rename_library", { libraryId, name });
  },

  async getAppInfo(): Promise<AppInfo> {
    if (!isTauri()) {
      return { version: "0.1.0", settingsPath: "(浏览器预览模式)" };
    }
    return invoke<AppInfo>("get_app_info");
  },

  async windowMinimize(): Promise<void> {
    if (!isTauri()) return;
    return invoke<void>("window_minimize");
  },

  async windowToggleMaximize(): Promise<boolean> {
    if (!isTauri()) return false;
    return invoke<boolean>("window_toggle_maximize");
  },

  async windowIsMaximized(): Promise<boolean> {
    if (!isTauri()) return false;
    return invoke<boolean>("window_is_maximized");
  },

  async windowClose(): Promise<void> {
    if (!isTauri()) return;
    return invoke<void>("window_close");
  },
};

export const browserPreview = !isTauri();
