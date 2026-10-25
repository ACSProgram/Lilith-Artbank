import { invoke } from "@tauri-apps/api/core";
import { mockLibraryCounts } from "../modules/library/mock";
import type {
  AppInfo,
  LibraryInfo,
  LibraryInspection,
  McpStatus,
  Settings,
} from "./types";

const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

const defaultSettings: Settings = {
  settingsVersion: 6,
  libraryPath: null,
  ai: {
    baseUrl: "http://127.0.0.1:18766",
    port: 18766,
    autoStart: true,
    pythonPath: "python_embeded\\python.exe",
    serverScript: "server\\main.py",
    modelPath: "resources\\models",
    clipModelPath: "siglip2-giant-opt-patch16-384",
  },
  performance: {
    readerConnections: 4,
    workerThreads: null,
  },
  history: {
    maxSteps: 100,
  },
  mcp: {
    enabled: false,
    port: 18767,
    mode: "ask",
  },
  ui: {
    theme: "system",
    gridThumbPx: 220,
    thumbFit: "cover",
    thumbCacheCount: 1024,
    sort: [{ key: "importedAt", descending: true }],
    sortProfiles: [],
    viewerFilmstrip: true,
  },
};

let mockSettings: Settings = structuredClone(defaultSettings);
let mockCurrent: LibraryInfo | null = null;

function basename(path: string): string {
  const parts = path.trim().replace(/[\\/]+$/, "").split(/[\\/]/);
  return parts[parts.length - 1] || "图库";
}

function mockLibrary(name: string, path: string): LibraryInfo {
  const counts = mockLibraryCounts();
  return {
    id: crypto.randomUUID(),
    name,
    path,
    imageCount: counts.imageCount,
    trashCount: counts.trashCount,
    schemaVersion: 2,
    openedAt: Date.now(),
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
      const library = mockLibrary(name || basename(path), path);
      mockCurrent = library;
      mockSettings = { ...mockSettings, libraryPath: path };
      return library;
    }
    return invoke<LibraryInfo>("create_library", { name, path });
  },

  async openLibraryPath(path: string): Promise<LibraryInfo> {
    if (!isTauri()) {
      const library = mockLibrary(basename(path), path);
      mockCurrent = library;
      mockSettings = { ...mockSettings, libraryPath: path };
      return library;
    }
    return invoke<LibraryInfo>("open_library_path", { path });
  },

  async closeLibrary(): Promise<void> {
    if (!isTauri()) {
      mockCurrent = null;
      mockSettings = { ...mockSettings, libraryPath: null };
      return;
    }
    return invoke<void>("close_library");
  },

  async getLibraryStatus(): Promise<LibraryInfo | null> {
    if (!isTauri()) {
      if (mockCurrent) {
        const counts = mockLibraryCounts();
        mockCurrent = {
          ...mockCurrent,
          imageCount: counts.imageCount,
          trashCount: counts.trashCount,
        };
      }
      return mockCurrent;
    }
    return invoke<LibraryInfo | null>("get_library_status");
  },

  async renameLibrary(name: string): Promise<LibraryInfo> {
    if (!isTauri()) {
      if (!mockCurrent) throw new Error("尚未打开图库");
      mockCurrent = { ...mockCurrent, name };
      return mockCurrent;
    }
    return invoke<LibraryInfo>("rename_library", { name });
  },

  async getAppInfo(): Promise<AppInfo> {
    if (!isTauri()) {
      return {
        version: "0.1.0",
        settingsPath: "(浏览器预览模式)",
        logPath: "(浏览器预览模式)",
      };
    }
    return invoke<AppInfo>("get_app_info");
  },

  async getMcpStatus(): Promise<McpStatus> {
    if (!isTauri()) {
      return {
        enabled: false,
        running: false,
        port: 18767,
        mode: "ask",
        runtimePath: "(浏览器预览模式)",
        error: null,
      };
    }
    return invoke<McpStatus>("get_mcp_status");
  },

  /** 在系统文件管理器里定位文件或打开目录。 */
  async revealPath(path: string): Promise<void> {
    if (!isTauri()) return;
    return invoke<void>("reveal_path", { path });
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

  async windowSetFullscreen(value: boolean): Promise<boolean> {
    if (!isTauri()) {
      if (value) await document.documentElement.requestFullscreen();
      else if (document.fullscreenElement) await document.exitFullscreen();
      return Boolean(document.fullscreenElement);
    }
    return invoke<boolean>("window_set_fullscreen", { value });
  },
};

export const browserPreview = !isTauri();
