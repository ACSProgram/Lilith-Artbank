import { useEffect, useRef, useState, type ReactNode } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Database,
  ExternalLink,
  FolderOpen,
  Gauge,
  Info,
  LogOut,
  Monitor,
  Moon,
  Palette,
  Plug,
  SlidersHorizontal,
  Sparkles,
  Sun,
} from "lucide-react";
import { Button } from "../shared/components/Button";
import { Field } from "../shared/components/Field";
import { Modal } from "../shared/components/Modal";
import { appApi, browserPreview } from "./api";
import { McpPanel } from "./McpPanel";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";
import { setThemeMode } from "./theme";
import type { McpStatus, Settings, ThemeMode, ThumbFit } from "./types";
import { errorMessage } from "../shared/errors";
import { aiApi, aiBrowserPreview } from "../modules/ai/api";
import type { AiStatus } from "../modules/ai/types";

const THEME_CHOICES: { value: ThemeMode; label: string; icon: typeof Sun }[] = [
  { value: "light", label: "浅色", icon: Sun },
  { value: "dark", label: "深色", icon: Moon },
  { value: "system", label: "跟随系统", icon: Monitor },
];

const FIT_CHOICES: { value: ThumbFit; label: string }[] = [
  { value: "cover", label: "裁剪铺满" },
  { value: "contain", label: "完整显示" },
];

type SettingsTab =
  | "general"
  | "appearance"
  | "library"
  | "ai"
  | "mcp"
  | "performance"
  | "about";

const TABS: { id: SettingsTab; label: string; icon: typeof Sun }[] = [
  { id: "general", label: "通用", icon: SlidersHorizontal },
  { id: "appearance", label: "外观", icon: Palette },
  { id: "library", label: "图库", icon: Database },
  { id: "ai", label: "AI 服务", icon: Sparkles },
  { id: "mcp", label: "MCP", icon: Plug },
  { id: "performance", label: "性能", icon: Gauge },
  { id: "about", label: "关于", icon: Info },
];

const TAB_STORAGE_KEY = "lilith.settingsTab";

const CAPABILITY_LABELS: Record<string, string> = {
  imageEmbedding: "图像塔",
  textEmbedding: "文本塔",
  pose: "姿势",
  description: "描述",
};

function initialTab(): SettingsTab {
  try {
    const saved = localStorage.getItem(TAB_STORAGE_KEY);
    if (TABS.some((tab) => tab.id === saved)) return saved as SettingsTab;
  } catch {
    // 忽略无法读取本地偏好的情况。
  }
  return "general";
}

function numberOrNull(value: string): number | null {
  if (!value.trim()) return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function Panel({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <section className="settings-panel">
      <header className="settings-panel-head">
        <h3>{title}</h3>
      </header>
      <div className="settings-panel-body">{children}</div>
    </section>
  );
}

/** 标签 + 右侧当前值；滑块这类“调多少”的控件用它，省掉另起一行解释单位。 */
function ValueField({
  label,
  value,
  children,
}: {
  label: string;
  value: string;
  children: ReactNode;
}) {
  return (
    <label className="settings-field">
      <span className="settings-field-head">
        <span className="field-label">{label}</span>
        <span className="settings-value">{value}</span>
      </span>
      {children}
    </label>
  );
}

export function SettingsDialog({
  open: isOpen,
  onClose,
  onChangeLibrary,
  onCloseLibrary,
}: {
  open: boolean;
  onClose: () => void;
  onChangeLibrary: () => void;
  onCloseLibrary: () => Promise<void>;
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
      return "工作线程数必须在 1-128 之间，或留空使用自动值";
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

  const capabilities = aiStatus?.health?.capabilities;
  const capabilityState = (name: string): string => {
    if (!aiStatus?.online) return "未连接";
    const capability = capabilities?.[name];
    if (!capability) return "未配置";
    return capability.ready ? "已加载" : "未加载";
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
                <Panel title="浏览">
                  <ValueField label="缩略图尺寸" value={`${draft.ui.gridThumbPx} px`}>
                    <input
                      className="range"
                      type="range"
                      min={96}
                      max={512}
                      step={8}
                      value={draft.ui.gridThumbPx}
                      onChange={(event) =>
                        patch((current) => ({
                          ...current,
                          ui: { ...current.ui, gridThumbPx: Number(event.target.value) },
                        }))
                      }
                    />
                  </ValueField>

                  <div className="settings-field">
                    <span className="field-label">缩略图填充</span>
                    <div className="settings-choices">
                      {FIT_CHOICES.map((choice) => (
                        <button
                          key={choice.value}
                          type="button"
                          className={`settings-choice${draft.ui.thumbFit === choice.value ? " is-active" : ""}`}
                          aria-pressed={draft.ui.thumbFit === choice.value}
                          onClick={() =>
                            patch((current) => ({
                              ...current,
                              ui: { ...current.ui, thumbFit: choice.value },
                            }))
                          }
                        >
                          {choice.label}
                        </button>
                      ))}
                    </div>
                  </div>

                  <ValueField label="缩略图缓存" value={`${draft.ui.thumbCacheCount} 张`}>
                    <input
                      className="range"
                      type="range"
                      min={256}
                      max={8192}
                      step={128}
                      value={draft.ui.thumbCacheCount}
                      onChange={(event) =>
                        patch((current) => ({
                          ...current,
                          ui: { ...current.ui, thumbCacheCount: Number(event.target.value) },
                        }))
                      }
                    />
                  </ValueField>

                  <Field
                    label="操作记录步数"
                    hint="减少后更早的记录会立即清理，永久删除的文件随之释放。"
                  >
                    <input
                      className="input"
                      type="number"
                      min={10}
                      max={500}
                      value={draft.history.maxSteps}
                      onChange={(event) =>
                        patch((current) => ({
                          ...current,
                          history: { ...current.history, maxSteps: Number(event.target.value) },
                        }))
                      }
                    />
                  </Field>
                </Panel>
              ) : null}

              {tab === "appearance" ? (
                <Panel title="主题">
                  <div className="settings-choices">
                    {THEME_CHOICES.map(({ value, label, icon: Icon }) => (
                      <button
                        key={value}
                        type="button"
                        className={`settings-choice${draft.ui.theme === value ? " is-active" : ""}`}
                        aria-pressed={draft.ui.theme === value}
                        onClick={() =>
                          patch((current) => ({ ...current, ui: { ...current.ui, theme: value } }))
                        }
                      >
                        <Icon size={15} />
                        {label}
                      </button>
                    ))}
                  </div>
                </Panel>
              ) : null}

              {tab === "library" ? (
                <Panel title="当前图库">
                  {current ? (
                    <div className="settings-library-card">
                      <Database size={16} />
                      <div className="settings-library-info">
                        <strong>{current.name}</strong>
                        <span title={current.path}>{current.path}</span>
                      </div>
                      <span className="settings-library-count">
                        {current.imageCount.toLocaleString()} 张
                      </span>
                    </div>
                  ) : (
                    <div className="settings-library-card">
                      <Database size={16} />
                      <div className="settings-library-info">
                        <strong>尚未打开图库</strong>
                      </div>
                    </div>
                  )}
                  <div className="settings-inline-actions">
                    <Button size="sm" variant="secondary" icon={<FolderOpen size={14} />} onClick={onChangeLibrary}>
                      {current ? "更换图库" : "打开图库"}
                    </Button>
                    {current ? (
                      <Button
                        size="sm"
                        variant="ghost"
                        icon={<ExternalLink size={14} />}
                        onClick={() => void revealPath(current.path, "打开图库目录失败")}
                      >
                        打开图库目录
                      </Button>
                    ) : null}
                    {current ? (
                      <Button
                        size="sm"
                        variant="danger"
                        icon={<LogOut size={14} />}
                        onClick={() => void closeCurrentLibrary()}
                      >
                        关闭图库
                      </Button>
                    ) : null}
                  </div>
                  <div className="settings-note">关闭不会删除任何图片或标签、评分等信息。</div>
                </Panel>
              ) : null}

              {tab === "ai" ? (
                <Panel title="AI 服务">
                  <div className="settings-grid">
                    <Field label="服务地址" hint="非本机地址时只连接外部服务。">
                      <input
                        className="input"
                        value={draft.ai.baseUrl}
                        onChange={(event) =>
                          patch((current) => ({
                            ...current,
                            ai: { ...current.ai, baseUrl: event.target.value },
                          }))
                        }
                        spellCheck={false}
                      />
                    </Field>
                    <Field label="端口">
                      <input
                        className="input"
                        type="number"
                        min={1}
                        max={65535}
                        value={draft.ai.port}
                        onChange={(event) => {
                          const port = Number(event.target.value);
                          patch((current) => {
                            const local = /^http:\/\/(127\.0\.0\.1|localhost):\d+$/i.test(
                              current.ai.baseUrl.trim(),
                            );
                            return {
                              ...current,
                              ai: {
                                ...current.ai,
                                port,
                                baseUrl:
                                  local && port > 0
                                    ? `http://127.0.0.1:${port}`
                                    : current.ai.baseUrl,
                              },
                            };
                          });
                        }}
                      />
                    </Field>
                  </div>
                  <Field label="Python" hint="相对程序目录；留空只连接外部服务。">
                    <div className="input-row">
                      <input
                        className="input"
                        value={draft.ai.pythonPath ?? ""}
                        onChange={(event) =>
                          patch((current) => ({
                            ...current,
                            ai: {
                              ...current.ai,
                              pythonPath: event.target.value.trim() ? event.target.value : null,
                            },
                          }))
                        }
                        placeholder={"python_embeded\\python.exe"}
                        spellCheck={false}
                      />
                      <Button
                        variant="secondary"
                        icon={<FolderOpen size={16} />}
                        onClick={() => void pickPath("python")}
                      >
                        选择
                      </Button>
                    </div>
                  </Field>
                  <Field label="服务脚本" hint="相对程序目录。">
                    <div className="input-row">
                      <input
                        className="input"
                        value={draft.ai.serverScript ?? ""}
                        onChange={(event) =>
                          patch((current) => ({
                            ...current,
                            ai: {
                              ...current.ai,
                              serverScript: event.target.value.trim() ? event.target.value : null,
                            },
                          }))
                        }
                        placeholder={"server\\main.py"}
                        spellCheck={false}
                      />
                      <Button
                        variant="secondary"
                        icon={<FolderOpen size={16} />}
                        onClick={() => void pickPath("script")}
                      >
                        选择
                      </Button>
                    </div>
                  </Field>
                  <Field label="模型目录" hint="相对程序目录。">
                    <div className="input-row">
                      <input
                        className="input"
                        value={draft.ai.modelPath ?? ""}
                        onChange={(event) =>
                          patch((current) => ({
                            ...current,
                            ai: {
                              ...current.ai,
                              modelPath: event.target.value.trim() ? event.target.value : null,
                            },
                          }))
                        }
                        placeholder={"resources\\models"}
                        spellCheck={false}
                      />
                      <Button
                        variant="secondary"
                        icon={<FolderOpen size={16} />}
                        onClick={() => void pickPath("models")}
                      >
                        选择
                      </Button>
                    </div>
                  </Field>
                  <Field label="CLIP 模型" hint="相对模型目录，或填绝对路径。">
                    <input
                      className="input"
                      value={draft.ai.clipModelPath ?? ""}
                      onChange={(event) =>
                        patch((current) => ({
                          ...current,
                          ai: {
                            ...current.ai,
                            clipModelPath: event.target.value.trim() ? event.target.value : null,
                          },
                        }))
                      }
                      placeholder="siglip2-giant-opt-patch16-384"
                      spellCheck={false}
                    />
                  </Field>
                  <label className="toggle-row">
                    <input
                      type="checkbox"
                      checked={draft.ai.autoStart}
                      onChange={(event) =>
                        patch((current) => ({
                          ...current,
                          ai: { ...current.ai, autoStart: event.target.checked },
                        }))
                      }
                    />
                    <span>
                      <strong>随应用启动 AI 服务</strong>
                      <small>只在服务地址是本机时启动。</small>
                    </span>
                  </label>
                  <label className="toggle-row">
                    <input
                      type="checkbox"
                      checked={draft.ai.prewarmImports}
                      onChange={(event) =>
                        patch((current) => ({
                          ...current,
                          ai: { ...current.ai, prewarmImports: event.target.checked },
                        }))
                      }
                    />
                    <span>
                      <strong>启动时预热模型依赖</strong>
                      <small>服务进程多占约 420 MB，首次搜索少等 8 秒左右。</small>
                    </span>
                  </label>
                  <div className="settings-note">路径与端口改动保存后生效。</div>
                  <div className="ai-settings-status">
                    <div>
                      <strong>
                        {aiStatus?.online
                          ? "服务在线"
                          : aiStatus?.process.state === "starting"
                            ? "服务启动中"
                            : "服务离线"}
                      </strong>
                      <span>
                        {aiError ??
                          aiStatus?.process.message ??
                          aiStatus?.baseUrl ??
                          "尚未检测服务"}
                      </span>
                      <span>
                        {["imageEmbedding", "textEmbedding", "pose"]
                          .map(
                            (name) =>
                              `${CAPABILITY_LABELS[name] ?? name}${capabilityState(name)}`,
                          )
                          .join(" · ")}
                      </span>
                    </div>
                    <div className="settings-inline-actions">
                      <Button size="sm" variant="ghost" loading={aiBusy} onClick={() => void refreshAi()}>
                        检测
                      </Button>
                      {aiStatus?.process.owned ? (
                        <Button
                          size="sm"
                          variant="ghost"
                          loading={aiBusy}
                          disabled={aiBrowserPreview}
                          onClick={() => void runAi("stop")}
                        >
                          停止服务
                        </Button>
                      ) : (
                        <Button
                          size="sm"
                          variant="secondary"
                          loading={aiBusy}
                          disabled={aiBrowserPreview || !aiStatus?.launchConfigured}
                          title={
                            aiStatus && !aiStatus.launchConfigured
                              ? "请填写本机服务地址、Python 与服务脚本"
                              : undefined
                          }
                          onClick={() => void runAi("start")}
                        >
                          启动服务
                        </Button>
                      )}
                    </div>
                  </div>
                  <div className="settings-note">
                    模型在第一次用到时自动加载；图像塔与文本塔轮流驻留，空闲 5 分钟后释放。
                  </div>
                  <div className="settings-note">服务离线时，AI 相关按钮不可用，浏览与编辑不受影响。</div>
                </Panel>
              ) : null}

              {tab === "mcp" ? (
                <Panel title="MCP 服务">
                  <McpPanel
                    draft={draft}
                    patch={patch}
                    status={mcpStatus}
                    statusError={mcpError}
                  />
                </Panel>
              ) : null}

              {tab === "performance" ? (
                <Panel title="性能">
                  <div className="settings-grid">
                    <Field label="读取连接数">
                      <input
                        className="input"
                        type="number"
                        min={1}
                        max={16}
                        value={draft.performance.readerConnections}
                        onChange={(event) =>
                          patch((current) => ({
                            ...current,
                            performance: {
                              ...current.performance,
                              readerConnections: Number(event.target.value),
                            },
                          }))
                        }
                      />
                    </Field>
                    <Field label="工作线程">
                      <input
                        className="input"
                        type="number"
                        min={1}
                        max={128}
                        value={draft.performance.workerThreads ?? ""}
                        onChange={(event) =>
                          patch((current) => ({
                            ...current,
                            performance: {
                              ...current.performance,
                              workerThreads: numberOrNull(event.target.value),
                            },
                          }))
                        }
                        placeholder="自动"
                      />
                    </Field>
                  </div>
                  <div className="settings-note">
                    <Gauge size={13} />
                    改动在下次打开图库后生效。
                  </div>
                </Panel>
              ) : null}

              {tab === "about" ? (
                <Panel title="关于">
                  <div className="settings-field">
                    <span className="field-label">设置文件</span>
                    <div className="settings-path-row">
                      <code className="settings-path">{appInfo?.settingsPath ?? "—"}</code>
                      <Button
                        size="sm"
                        variant="secondary"
                        icon={<ExternalLink size={13} />}
                        disabled={!appInfo?.settingsPath}
                        onClick={() => void revealPath(appInfo?.settingsPath, "打开设置文件所在文件夹失败")}
                      >
                        打开文件夹
                      </Button>
                    </div>
                  </div>
                  <div className="settings-field">
                    <span className="field-label">日志文件</span>
                    <div className="settings-path-row">
                      <code className="settings-path">{appInfo?.logPath || "—"}</code>
                      <Button
                        size="sm"
                        variant="secondary"
                        icon={<ExternalLink size={13} />}
                        disabled={!appInfo?.logPath}
                        onClick={() => void revealPath(appInfo?.logPath, "打开日志文件夹失败")}
                      >
                        打开文件夹
                      </Button>
                    </div>
                  </div>
                  <div className="settings-note">
                    <Info size={13} />
                    删掉设置文件会回到默认值，图库内容不受影响。
                  </div>
                </Panel>
              ) : null}

              {error ? <div className="form-error">{error}</div> : null}
            </>
          )}
        </div>
      </div>
    </Modal>
  );
}
