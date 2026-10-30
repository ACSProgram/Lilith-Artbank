import { FolderOpen } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { Field } from "../../shared/components/Field";
import { aiBrowserPreview } from "../../modules/ai/api";
import type { AiStatus } from "../../modules/ai/types";
import type { Settings } from "../types";
import { Panel, type SettingsPatch } from "./SettingsPanel";

const CAPABILITY_LABELS: Record<string, string> = {
  imageEmbedding: "图像塔",
  textEmbedding: "文本塔",
  pose: "姿势",
  description: "描述",
};

export function AiSettingsSection({
  draft,
  patch,
  aiStatus,
  aiError,
  aiBusy,
  onRefresh,
  onRun,
  onPickPath,
}: {
  draft: Settings;
  patch: SettingsPatch;
  aiStatus: AiStatus | null;
  aiError: string | null;
  aiBusy: boolean;
  onRefresh: () => void;
  onRun: (action: "start" | "stop") => void;
  onPickPath: (kind: "python" | "script" | "models") => void;
}) {
  const capabilities = aiStatus?.health?.capabilities;
  const capabilityState = (name: string): string => {
    if (!aiStatus?.online) return "未连接";
    const capability = capabilities?.[name];
    if (!capability) return "未配置";
    return capability.ready ? "已加载" : "未加载";
  };

  return (
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
            onClick={() => onPickPath("python")}
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
            onClick={() => onPickPath("script")}
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
            onClick={() => onPickPath("models")}
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
          <Button size="sm" variant="ghost" loading={aiBusy} onClick={onRefresh}>
            检测
          </Button>
          {aiStatus?.process.owned ? (
            <Button
              size="sm"
              variant="ghost"
              loading={aiBusy}
              disabled={aiBrowserPreview}
              onClick={() => onRun("stop")}
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
              onClick={() => onRun("start")}
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
  );
}
