import { useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Database,
  Gauge,
  Info,
  Palette,
  Plug,
  SlidersHorizontal,
  Sparkles,
} from "lucide-react";
import { Button } from "../shared/components/Button";
import { Modal } from "../shared/components/Modal";
import { appApi, browserPreview } from "./api";
import { useNotices } from "../shared/notices";
import { useAppRuntime } from "./runtimeContext";
import { setThemeMode } from "./theme";
import type { McpStatus, Settings } from "./types";
import { errorMessage } from "../shared/errors";
import { aiApi } from "../modules/ai/api";
import type { AiStatus } from "../modules/ai/types";
import { AboutSettingsSection } from "./settings/AboutSettingsSection";
import { AiSettingsSection } from "./settings/AiSettingsSection";
import { AppearanceSettingsSection } from "./settings/AppearanceSettingsSection";
import { GeneralSettingsSection } from "./settings/GeneralSettingsSection";
import { LibrarySettingsSection } from "./settings/LibrarySettingsSection";
import { MaintenanceSettingsSection } from "./settings/MaintenanceSettingsSection";
import { McpSettingsSection } from "./settings/McpSettingsSection";
import { PerformanceSettingsSection } from "./settings/PerformanceSettingsSection";

type SettingsTab =
  | "general"
  | "appearance"
  | "library"
  | "ai"
  | "mcp"
  | "performance"
  | "about";

const TABS: { id: SettingsTab; label: string; icon: typeof SlidersHorizontal }[] = [
  { id: "general", label: "通用", icon: SlidersHorizontal },
  { id: "appearance", label: "外观", icon: Palette },
  { id: "library", label: "图库", icon: Database },
  { id: "ai", label: "AI 服务", icon: Sparkles },
  { id: "mcp", label: "MCP", icon: Plug },
  { id: "performance", label: "性能", icon: Gauge },
  { id: "about", label: "关于", icon: Info },
];

const TAB_STORAGE_KEY = "lilith.settingsTab";

function initialTab(): SettingsTab {
  try {
    const saved = localStorage.getItem(TAB_STORAGE_KEY);
    if (TABS.some((tab) => tab.id === saved)) return saved as SettingsTab;
  } catch {
    // 忽略无法读取本地偏好的情况。
  }
  return "general";
}

export function SettingsDialog({
  open: isOpen,
  onClose,
  onChangeLibrary,
  onCloseLibrary,
  onOpenShortcuts,
}: {
  open: boolean;
  onClose: () => void;
  onChangeLibrary: () => void;
  onCloseLibrary: () => Promise<void>;
  /** 打开只读的快捷键总表；由 AppShell 持有，`?` 与这里的按钮是同一个入口。 */
  onOpenShortcuts: () => void;
}) {
  const { settings, current, appInfo, saveSettings } = useAppRuntime();
  const { notify, confirm } = useNotices();
  const [draft, setDraft] = useState<Settings | null>(null);
  const [tab, setTab] = useState<SettingsTab>(initialTab);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [aiStatus, setAiStatus] = useState<AiStatus | null>(null);
  const [aiBusy, setAiBusy] = useState(false);
  const [aiError, setAiError] = useState<string | null>(null);
  const [mcpStatus, setMcpStatus] = useState<McpStatus | null>(null);
  const [mcpError, setMcpError] = useState<string | null>(null);
  const contentRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!isOpen) return;
    setDraft(settings ? structuredClone(settings) : null);
    setError(null);
    setSaving(false);
    setAiError(null);
    setMcpError(null);
    void aiApi
      .getStatus()
      .then(setAiStatus)
      .catch((cause: unknown) => setAiError(errorMessage(cause)));
    void appApi
      .getMcpStatus()
      .then(setMcpStatus)
      .catch((cause: unknown) => setMcpError(errorMessage(cause)));
  }, [isOpen, settings]);

  useEffect(() => {
    try {
      localStorage.setItem(TAB_STORAGE_KEY, tab);
    } catch {
      // 忽略无法持久化分类偏好的情况。
    }
    // 切换分类从头开始读，避免沿用上一个分类的滚动位置。
    if (contentRef.current) contentRef.current.scrollTop = 0;
  }, [tab]);

  // 主题即时预览：改选项马上生效，取消则退回已保存的值。
  const draftTheme = draft?.ui.theme;
  const savedTheme = settings?.ui.theme ?? "system";
  useEffect(() => {
    if (!isOpen || !draftTheme) return;
    setThemeMode(draftTheme);
    return () => setThemeMode(savedTheme);
  }, [isOpen, draftTheme, savedTheme]);

  const refreshAi = async () => {
    setAiBusy(true);
    try {
      setAiStatus(await aiApi.getStatus());
      setAiError(null);
    } catch (cause) {
      setAiError(errorMessage(cause));
    } finally {
      setAiBusy(false);
    }
  };

  const runAi = async (action: "start" | "stop") => {
    setAiBusy(true);
    setAiError(null);
    try {
      if (action === "start") await aiApi.startService();
      if (action === "stop") await aiApi.stopService();
      await refreshAi();
    } catch (cause) {
      setAiError(errorMessage(cause));
    } finally {
      setAiBusy(false);
    }
  };

  const patch = (update: (current: Settings) => Settings) => {
    setDraft((current) => (current ? update(structuredClone(current)) : current));
  };

  const pickPath = async (kind: "python" | "script" | "models") => {
    if (browserPreview) {
      patch((current) => ({
        ...current,
        ai:
          kind === "python"
            ? { ...current.ai, pythonPath: "python_embeded\\python.exe" }
            : kind === "script"
              ? { ...current.ai, serverScript: "server\\main.py" }
              : { ...current.ai, modelPath: "resources\\models" },
      }));
      return;
    }
    try {
      const selected = await open({
        multiple: false,
        directory: kind === "models",
        title:
          kind === "python"
            ? "选择 Python"
            : kind === "script"
              ? "选择 AI 服务脚本"
              : "选择模型目录",
        filters:
          kind === "python"
            ? [{ name: "Python", extensions: ["exe"] }]
            : kind === "script"
              ? [{ name: "Python 脚本", extensions: ["py"] }]
              : undefined,
      });
      if (typeof selected !== "string") return;
      patch((current) => ({
        ...current,
        ai:
          kind === "python"
            ? { ...current.ai, pythonPath: selected }
            : kind === "script"
              ? { ...current.ai, serverScript: selected }
              : { ...current.ai, modelPath: selected },
      }));
    } catch (cause) {
      notify("选择路径失败", "error", errorMessage(cause));
    }
  };

  const closeCurrentLibrary = async () => {
    const accepted = await confirm({
      title: "关闭图库？",
      message: "关闭后可以随时重新打开，不会修改图库内容。",
      confirmLabel: "关闭图库",
    });
    if (!accepted) return;
    try {
      await onCloseLibrary();
      onClose();
    } catch (cause) {
      notify("关闭图库失败", "error", errorMessage(cause));
    }
  };

  const revealPath = async (path: string | undefined, failure: string) => {
    if (!path) return;
    try {
      await appApi.revealPath(path);
    } catch (cause) {
      notify(failure, "error", errorMessage(cause));
    }
  };

  const validate = (value: Settings): string | null => {
    if (!/^https?:\/\//.test(value.ai.baseUrl.trim())) {
      return "AI 服务地址必须以 http:// 或 https:// 开头";
    }
    if (!Number.isInteger(value.ai.port) || value.ai.port < 1 || value.ai.port > 65535) {
      return "AI 服务端口必须在 1-65535 之间";
    }
    if (value.ai.pythonPath?.trim() && !value.ai.serverScript?.trim()) {
      return "填写 Python 路径时也要填写服务脚本";
    }
    const localUrl = value.ai.baseUrl.trim().match(/^http:\/\/(127\.0\.0\.1|localhost):(\d+)/i);
    if (localUrl && Number(localUrl[2]) !== value.ai.port) {
      return "服务地址端口与启动端口不一致";
    }
    if (
      !Number.isInteger(value.performance.readerConnections) ||
      value.performance.readerConnections < 1 ||
      value.performance.readerConnections > 16
    ) {
      return "读取连接数必须在 1-16 之间";
    }
    if (
      value.performance.workerThreads !== null &&
      (!Number.isInteger(value.performance.workerThreads) ||
        value.performance.workerThreads < 1 ||
        value.performance.workerThreads > 128)
    ) {
      return "后台图像工作线程数必须在 1-128 之间，或留空使用自动值";
    }
    if (
      !Number.isInteger(value.ui.gridThumbPx) ||
      value.ui.gridThumbPx < 96 ||
      value.ui.gridThumbPx > 512
    ) {
      return "网格缩略图尺寸必须在 96-512 像素之间";
    }
    if (
      !Number.isInteger(value.ui.thumbCacheCount) ||
      value.ui.thumbCacheCount < 256 ||
      value.ui.thumbCacheCount > 8192
    ) {
      return "缩略图缓存张数必须在 256-8192 之间";
    }
    if (
      !Number.isInteger(value.history.maxSteps) ||
      value.history.maxSteps < 10 ||
      value.history.maxSteps > 500
    ) {
      return "操作记录步数必须在 10-500 之间";
    }
    if (!Number.isInteger(value.mcp.port) || value.mcp.port < 1 || value.mcp.port > 65535) {
      return "MCP 端口必须在 1-65535 之间";
    }
    return null;
  };

  const save = async () => {
    if (!draft) return;
    const validation = validate(draft);
    if (validation) {
      setError(validation);
      return;
    }
    setSaving(true);
    setError(null);
    try {
      await saveSettings(draft);
      onClose();
    } catch (cause) {
      const message = errorMessage(cause);
      setError(message);
      notify("保存设置失败", "error", message);
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      open={isOpen}
      title="设置"
      onClose={onClose}
      width={700}
      // 固定尺寸 + 内容区自己滚动：切换分类、状态刷新、报错都不改变对话框大小。
      height={560}
      bodyClassName="modal-body-fixed"
      dismissible={!saving}
      footer={
        <>
          <Button variant="ghost" onClick={onClose} disabled={saving}>
            取消
          </Button>
          <Button variant="primary" onClick={() => void save()} loading={saving} disabled={!draft}>
            保存设置
          </Button>
        </>
      }
    >
      <div className="settings-body">
        <nav className="settings-nav" aria-label="设置分类">
          {TABS.map(({ id, label, icon: Icon }) => (
            <button
              key={id}
              type="button"
              className={`settings-nav-item${tab === id ? " is-active" : ""}`}
              aria-current={tab === id}
              onClick={() => setTab(id)}
            >
              <Icon size={15} />
              <span>{label}</span>
            </button>
          ))}
        </nav>

        <div className="settings-content" ref={contentRef}>
          {!draft ? (
            <p className="form-note">正在读取设置…</p>
          ) : (
            <>
              {tab === "general" ? (
                <>
                  <GeneralSettingsSection draft={draft} patch={patch} />
                  <MaintenanceSettingsSection
                    draft={draft}
                    patch={patch}
                    onOpenShortcuts={onOpenShortcuts}
                  />
                </>
              ) : null}

              {tab === "appearance" ? (
                <AppearanceSettingsSection draft={draft} patch={patch} />
              ) : null}

              {tab === "library" ? (
                <LibrarySettingsSection
                  current={current}
                  onChangeLibrary={onChangeLibrary}
                  onRevealPath={(path, failure) => void revealPath(path, failure)}
                  onCloseLibrary={() => void closeCurrentLibrary()}
                />
              ) : null}

              {tab === "ai" ? (
                <AiSettingsSection
                  draft={draft}
                  patch={patch}
                  aiStatus={aiStatus}
                  aiError={aiError}
                  aiBusy={aiBusy}
                  onRefresh={() => void refreshAi()}
                  onRun={(action) => void runAi(action)}
                  onPickPath={(kind) => void pickPath(kind)}
                />
              ) : null}

              {tab === "mcp" ? (
                <McpSettingsSection
                  draft={draft}
                  patch={patch}
                  status={mcpStatus}
                  statusError={mcpError}
                />
              ) : null}

              {tab === "performance" ? (
                <PerformanceSettingsSection draft={draft} patch={patch} />
              ) : null}

              {tab === "about" ? (
                <AboutSettingsSection
                  appInfo={appInfo}
                  onRevealPath={(path, failure) => void revealPath(path, failure)}
                />
              ) : null}

              {error ? <div className="form-error">{error}</div> : null}
            </>
          )}
        </div>
      </div>
    </Modal>
  );
}
