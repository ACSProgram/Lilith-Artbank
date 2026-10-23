import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { Database, FolderOpen, Gauge, Sparkles } from "lucide-react";
import { Button } from "../shared/components/Button";
import { Field, Section } from "../shared/components/Field";
import { Modal } from "../shared/components/Modal";
import { browserPreview } from "./api";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";
import type { Settings } from "./types";
import { errorMessage } from "../shared/errors";
import { aiApi, aiBrowserPreview } from "../modules/ai/api";
import type { AiStatus } from "../modules/ai/types";

function numberOrNull(value: string): number | null {
  if (!value.trim()) return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

export function SettingsDialog({ open: isOpen, onClose }: { open: boolean; onClose: () => void }) {
  const { settings, appInfo, saveSettings } = useAppRuntime();
  const { notify } = useNotices();
  const [draft, setDraft] = useState<Settings | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [aiStatus, setAiStatus] = useState<AiStatus | null>(null);
  const [aiBusy, setAiBusy] = useState(false);
  const [aiError, setAiError] = useState<string | null>(null);

  useEffect(() => {
    if (!isOpen) return;
    setDraft(settings ? structuredClone(settings) : null);
    setError(null);
    setSaving(false);
    setAiError(null);
    void aiApi
      .getStatus()
      .then(setAiStatus)
      .catch((cause: unknown) => setAiError(errorMessage(cause)));
  }, [isOpen, settings]);

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

  const runAi = async (action: "start" | "stop" | "ensure") => {
    setAiBusy(true);
    setAiError(null);
    try {
      if (action === "start") await aiApi.startService();
      if (action === "stop") await aiApi.stopService();
      if (action === "ensure") {
        await aiApi.ensureModels([
          "imageEmbedding",
          "textEmbedding",
          "pose",
          "description",
        ]);
      }
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

  const pickExecutable = async () => {
    if (browserPreview) {
      patch((current) => ({
        ...current,
        ai: { ...current.ai, executablePath: "C:\\Lilith Server\\ai-server.exe" },
      }));
      return;
    }
    try {
      const selected = await open({
        multiple: false,
        title: "选择 AI 服务可执行文件",
        filters: [{ name: "可执行文件", extensions: ["exe"] }],
      });
      if (typeof selected === "string") {
        patch((current) => ({
          ...current,
          ai: { ...current.ai, executablePath: selected },
        }));
      }
    } catch (cause) {
      notify("选择文件失败", "error", errorMessage(cause));
    }
  };

  const validate = (value: Settings): string | null => {
    if (!/^https?:\/\//.test(value.ai.baseUrl.trim())) {
      return "AI 服务地址必须以 http:// 或 https:// 开头";
    }
    if (!Number.isInteger(value.ai.port) || value.ai.port < 1 || value.ai.port > 65535) {
      return "AI 服务端口必须在 1-65535 之间";
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
      !Number.isInteger(value.performance.thumbnailCacheMb) ||
      value.performance.thumbnailCacheMb < 64 ||
      value.performance.thumbnailCacheMb > 4096
    ) {
      return "缩略图缓存必须在 64-4096 MB 之间";
    }
    if (
      !Number.isInteger(value.ui.gridThumbPx) ||
      value.ui.gridThumbPx < 96 ||
      value.ui.gridThumbPx > 512
    ) {
      return "网格缩略图尺寸必须在 96-512 像素之间";
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
      subtitle="设置保存在本机；AI 服务参数在下次检测或启动时生效。"
      onClose={onClose}
      width={720}
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
      {!draft ? (
        <p className="form-note">正在读取设置…</p>
      ) : (
        <div className="settings-body">
          <Section
            title="图库"
            description="图库的添加、切换与移除在左侧栏完成；这里只显示当前登记的路径。"
          >
            <div className="settings-library-list">
              {draft.libraries.length === 0 ? (
                <p className="form-note">尚未添加图库。</p>
              ) : (
                draft.libraries.map((library) => (
                  <div key={library.id} className="settings-library-row">
                    <Database size={15} />
                    <span className="settings-library-name">{library.name}</span>
                    <span className="settings-library-path" title={library.path}>
                      {library.path}
                    </span>
                  </div>
                ))
              )}
            </div>
          </Section>

          <Section
            title="AI 服务"
            description="description / embedding / pose 都走本地 HTTP 服务；embedding 为 SigLIP2 1536 维，服务返回模型名与维度。"
          >
            <div className="settings-grid">
              <Field label="服务地址" hint="默认本机 127.0.0.1:18766。">
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
              <Field label="端口" hint="用于托管服务时探活与启动参数。">
                <input
                  className="input"
                  type="number"
                  min={1}
                  max={65535}
                  value={draft.ai.port}
                  onChange={(event) =>
                    patch((current) => ({
                      ...current,
                      ai: { ...current.ai, port: Number(event.target.value) },
                    }))
                  }
                />
              </Field>
            </div>
            <Field label="服务可执行文件" hint="留空时只连接外部已启动的服务。">
              <div className="input-row">
                <input
                  className="input"
                  value={draft.ai.executablePath ?? ""}
                  onChange={(event) =>
                    patch((current) => ({
                      ...current,
                      ai: {
                        ...current.ai,
                        executablePath: event.target.value.trim() ? event.target.value : null,
                      },
                    }))
                  }
                  placeholder={"例如 C:\\Lilith Server\\bin\\server.exe"}
                  spellCheck={false}
                />
                <Button
                  variant="secondary"
                  icon={<FolderOpen size={16} />}
                  onClick={() => void pickExecutable()}
                >
                  选择
                </Button>
              </div>
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
                <small>接入后由应用管理进程、端口探活与退出顺序。</small>
              </span>
            </label>
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
                {aiStatus?.health ? (
                  <span>
                    {Object.entries(aiStatus.health.capabilities)
                      .map(
                        ([name, capability]) =>
                          `${name}: ${capability.ready ? "就绪" : "未加载"}${
                            capability.dim ? ` (${capability.dim})` : ""
                          }`,
                      )
                      .join(" · ")}
                  </span>
                ) : null}
              </div>
              <div className="settings-inline-actions">
                <Button size="sm" variant="ghost" loading={aiBusy} onClick={() => void refreshAi()}>
                  检测
                </Button>
                <Button
                  size="sm"
                  variant="secondary"
                  loading={aiBusy}
                  disabled={aiBrowserPreview}
                  onClick={() => void runAi("ensure")}
                >
                  加载模型
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
                    disabled={aiBrowserPreview}
                    onClick={() => void runAi("start")}
                  >
                    启动服务
                  </Button>
                )}
              </div>
            </div>
            <div className="settings-inline-note">
              <Sparkles size={14} />
              离线时 AI 按钮会提示错误，图库浏览、导入与编辑不受影响。
            </div>
          </Section>

          <Section
            title="性能"
            description="元数据只查询当前页；读取连接用于并发查询，写连接始终只有一条。"
          >
            <div className="settings-grid">
              <Field label="读取连接数" hint="1-16，默认 4。">
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
              <Field label="工作线程" hint="留空 = CPU 并行度 - 1。">
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
              <Field label="缩略图缓存 (MB)" hint="浏览器与 Rust 侧共享的预算，接入后生效。">
                <input
                  className="input"
                  type="number"
                  min={64}
                  max={4096}
                  value={draft.performance.thumbnailCacheMb}
                  onChange={(event) =>
                    patch((current) => ({
                      ...current,
                      performance: {
                        ...current.performance,
                        thumbnailCacheMb: Number(event.target.value),
                      },
                    }))
                  }
                />
              </Field>
              <Field label="网格缩略图尺寸" hint={`${draft.ui.gridThumbPx}px`}>
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
              </Field>
            </div>
            <div className="settings-inline-note">
              <Gauge size={14} />
              性能门槛与实测方法见 docs/guides/performance.md。
            </div>
          </Section>

          <Section title="界面" description="当前使用 Lilith Artworks 风格的浅色工作台。">
            <div className="theme-row">
              <span className="theme-swatch" />
              <div>
                <strong>浅色</strong>
                <small>暖白底、深绿强调色，减少界面对素材颜色的干扰。</small>
              </div>
            </div>
          </Section>

          {error ? <div className="form-error">{error}</div> : null}
          <p className="settings-path">设置文件：{appInfo?.settingsPath ?? "—"}</p>
        </div>
      )}
    </Modal>
  );
}
