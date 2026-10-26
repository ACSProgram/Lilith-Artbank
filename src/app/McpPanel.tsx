import { useCallback, useEffect, useState } from "react";
import { ShieldAlert } from "lucide-react";
import { Button } from "../shared/components/Button";
import { Field } from "../shared/components/Field";
import { copyText } from "../shared/clipboard";
import { errorMessage } from "../shared/errors";
import { libraryApi } from "../modules/library/api";
import type { HistoryEntry } from "../modules/library/types";
import { browserPreview } from "./api";
import { ApprovalCard, useApprovalActions, useMcpApprovals } from "./McpApprovalCard";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";
import type { McpMode, McpStatus, Settings } from "./types";

const MODE_CHOICES: { value: McpMode; label: string; hint: string }[] = [
  {
    value: "readOnly",
    label: "只读",
    hint: "AI 只能浏览、搜索、看详情与拼图。",
  },
  {
    value: "ask",
    label: "询问",
    hint: "导入、回收站、撤销/重做和超过 100 张的批量写入需要你确认。",
  },
  {
    value: "full",
    label: "完全",
    hint: "不再逐条确认，直接执行。",
  },
];

const HISTORY_STATUS_LABELS: Record<string, string> = {
  applied: "已生效",
  undone: "已撤销",
  redoing: "重做中",
  undoFailed: "撤销失败",
};

function formatTime(value: number): string {
  return new Date(value).toLocaleString(undefined, {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function McpPanel({
  draft,
  patch,
  status,
  statusError,
}: {
  draft: Settings;
  patch: (updater: (current: Settings) => Settings) => void;
  status: McpStatus | null;
  statusError: string | null;
}) {
  const { current, appInfo } = useAppRuntime();
  const libraryId = current?.id ?? null;
  const { notify } = useNotices();
  const {
    approvals,
    error: approvalsError,
    refresh: refreshApprovals,
  } = useMcpApprovals(draft.mcp.enabled);
  const [recent, setRecent] = useState<HistoryEntry[]>([]);
  const [recentError, setRecentError] = useState<string | null>(null);

  const loadRecent = useCallback(async () => {
    if (!libraryId || browserPreview) {
      setRecent([]);
      return;
    }
    try {
      const page = await libraryApi.getOperationHistory();
      setRecent(
        page.entries.filter((entry) => entry.label.startsWith("AI · ")).slice(0, 8),
      );
      setRecentError(null);
    } catch (cause) {
      setRecent([]);
      setRecentError(errorMessage(cause));
    }
  }, [libraryId]);

  const settled = useCallback(() => {
    void refreshApprovals();
    void loadRecent();
  }, [refreshApprovals, loadRecent]);
  const { busyId, approve, reject } = useApprovalActions(settled);

  useEffect(() => {
    void loadRecent();
    if (browserPreview) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      if (disposed) return;
      const changed = await listen("library-changed", () => void loadRecent());
      const finished = await listen("task-finished", () => void loadRecent());
      unlisten = () => {
        changed();
        finished();
      };
    })();
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [loadRecent]);

  const pending = approvals.filter(
    (approval) => approval.status === "pending" && approval.expiresAt > Date.now(),
  );

  const copyClientConfig = async () => {
    const command = appInfo?.exePath || "lilith-artbank.exe";
    const config = JSON.stringify(
      { mcpServers: { "lilith-artbank": { command, args: ["--mcp-stdio"] } } },
      null,
      2,
    );
    try {
      await copyText(config);
      notify("客户端配置已复制", "success");
    } catch (cause) {
      notify("复制客户端配置失败", "error", errorMessage(cause));
    }
  };

  return (
    <>
      <label className="toggle-row">
        <input
          type="checkbox"
          checked={draft.mcp.enabled}
          onChange={(event) =>
            patch((current) => ({
              ...current,
              mcp: { ...current.mcp, enabled: event.target.checked },
            }))
          }
        />
        <span>
          <strong>启用 MCP 服务</strong>
          <small>只从本机 127.0.0.1 接受连接，AI 客户端通过同一份图库状态读写。</small>
        </span>
      </label>

      <Field label="端口">
        <input
          className="input"
          type="number"
          min={1}
          max={65535}
          value={draft.mcp.port}
          onChange={(event) =>
            patch((current) => ({
              ...current,
              mcp: { ...current.mcp, port: Number(event.target.value) },
            }))
          }
        />
      </Field>

      <div className="settings-field">
        <span className="field-label">AI 权限</span>
        <div className="settings-choices">
          {MODE_CHOICES.map((choice) => (
            <button
              key={choice.value}
              type="button"
              className={`settings-choice${draft.mcp.mode === choice.value ? " is-active" : ""}`}
              aria-pressed={draft.mcp.mode === choice.value}
              onClick={() =>
                patch((current) => ({
                  ...current,
                  mcp: { ...current.mcp, mode: choice.value },
                }))
              }
            >
              {choice.label}
            </button>
          ))}
        </div>
        <div className="settings-note">
          {MODE_CHOICES.find((choice) => choice.value === draft.mcp.mode)?.hint}
        </div>
      </div>

      {draft.mcp.mode === "full" ? (
        <div className="mcp-warning">
          <ShieldAlert size={15} />
          <span>
            AI 可以读取任意本机路径并导入图片（不会压缩），可以修改、移动、移入回收站并撤销
            你自己的操作记录，且不会再逐条询问。没有永久删除、清空回收站、压缩与像素编辑工具。
          </span>
        </div>
      ) : null}

      <div className="settings-note">保存后生效。</div>
      <div className="ai-settings-status">
        <div>
          <strong>
            {status?.running
              ? "服务运行中"
              : status?.enabled
                ? "服务未运行"
                : "服务已关闭"}
          </strong>
          <span>
            {statusError ??
              status?.error ??
              `127.0.0.1:${status?.port ?? draft.mcp.port}`}
          </span>
        </div>
      </div>

      <div className="mcp-section">
        <div className="mcp-section-head">
          <strong>待确认</strong>
          <span>{pending.length} 条</span>
        </div>
        {approvalsError ? (
          <div className="settings-note">{approvalsError}</div>
        ) : pending.length === 0 ? (
          <div className="settings-note">没有等待处理的操作。</div>
        ) : (
          <div className="mcp-approval-list">
            {pending.map((approval) => (
              <ApprovalCard
                key={approval.id}
                approval={approval}
                busy={busyId !== null}
                onApprove={approve}
                onReject={reject}
              />
            ))}
          </div>
        )}
      </div>

      <div className="mcp-section">
        <div className="mcp-section-head">
          <strong>最近 AI 操作</strong>
          <Button size="sm" variant="ghost" onClick={() => void loadRecent()}>
            刷新
          </Button>
        </div>
        {recentError ? (
          <div className="settings-note">{recentError}</div>
        ) : recent.length === 0 ? (
          <div className="settings-note">还没有 AI 写入记录。</div>
        ) : (
          <ul className="mcp-recent-list">
            {recent.map((entry) => (
              <li key={entry.id}>
                <div>
                  <strong>{entry.label}</strong>
                  <span>{formatTime(entry.createdAt)}</span>
                </div>
                <span>
                  {HISTORY_STATUS_LABELS[entry.status] ?? entry.status}
                  {entry.affected > 0 ? ` · 影响 ${entry.affected.toLocaleString()}` : ""}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="settings-note">
        支持 stdio 的客户端用本程序加 --mcp-stdio 启动。
      </div>
      <div className="settings-inline-actions">
        <Button
          size="sm"
          variant="ghost"
          disabled={browserPreview}
          onClick={() => void copyClientConfig()}
        >
          复制客户端配置
        </Button>
      </div>
    </>
  );
}
