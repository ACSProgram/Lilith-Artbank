import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ShieldCheck, ShieldX } from "lucide-react";
import { appApi, browserPreview } from "./api";
import { useNotices } from "./noticeContext";
import { errorMessage } from "../shared/errors";
import { Button } from "../shared/components/Button";
import type { McpApproval, McpApprovalStatus } from "./types";

const STATUS_LABELS: Record<McpApprovalStatus, string> = {
  pending: "等待确认",
  running: "执行中",
  completed: "已完成",
  rejected: "已拒绝",
  expired: "已过期",
  failed: "失败",
};

interface ApprovalPreview {
  operation?: string;
  target?: string | null;
  count?: number;
  /** 导入按输入路径计数，其余写入按图片计数。 */
  unit?: "path" | "image";
  pathTotal?: number;
  paths?: string[];
  tags?: string[];
  score?: number;
  duplicatePolicy?: string;
  selected?: number;
  willChange?: number;
  steps?: number;
  affected?: number;
  blockedReason?: string | null;
  consequences?: string[];
  sample?: Array<{
    id: number;
    folderPath?: string;
    score?: number;
  }>;
  entries?: Array<{
    id: number;
    label: string;
    affected: number;
    status: string;
  }>;
}

function readPreview(value: unknown): ApprovalPreview {
  return value && typeof value === "object" ? (value as ApprovalPreview) : {};
}

function formatTime(value: number): string {
  return new Date(value).toLocaleString(undefined, {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function amountLine(preview: ApprovalPreview): string {
  const count = preview.count ?? preview.selected;
  if (typeof count === "number") {
    const target = preview.target ? ` · ${preview.target}` : "";
    const unit = preview.unit === "path" ? "个路径" : "张";
    return `${count.toLocaleString()} ${unit}${target}`;
  }
  if (typeof preview.steps === "number") {
    return `${preview.steps.toLocaleString()} 条记录 · 影响 ${(preview.affected ?? 0).toLocaleString()} 张`;
  }
  return "—";
}

/** 导入卡片的标签/评分/重复策略；三项都没有时不占一行。 */
function importMeta(preview: ApprovalPreview): string[] {
  return [
    preview.tags?.length ? `标签 ${preview.tags.join("、")}` : null,
    typeof preview.score === "number" && preview.score > 0 ? `评分 ${preview.score} 星` : null,
    preview.duplicatePolicy
      ? preview.duplicatePolicy === "merge"
        ? "重复项合并"
        : "跳过重复项"
      : null,
  ].filter((item): item is string => item !== null);
}

/** 列表只展开前几条，其余合并成一行。 */
function Remaining({ total, shown, label }: { total: number; shown: number; label: string }) {
  const rest = total - shown;
  if (rest <= 0) return null;
  return (
    <li className="mcp-approval-more">
      其余 {rest.toLocaleString()} {label}
    </li>
  );
}

export function ApprovalCard({
  approval,
  busy,
  onApprove,
  onReject,
}: {
  approval: McpApproval;
  busy: boolean;
  onApprove: (approval: McpApproval) => void;
  onReject: (approval: McpApproval) => void;
}) {
  const result =
    approval.result && typeof approval.result === "object"
      ? (approval.result as { latest?: unknown })
      : null;
  // 批准时复核失败会带回最新预览；卡片显示新数量，而不是过期的那份。
  const preview = readPreview(result?.latest ?? approval.preview);
  const sample = (preview.sample ?? []).slice(0, 5);
  const entries = (preview.entries ?? []).slice(0, 5);
  const paths = (preview.paths ?? []).slice(0, 5);
  const meta = importMeta(preview);
  const consequences = preview.consequences ?? [];
  const open = approval.status === "pending";

  return (
    <article className={`mcp-approval-card is-${approval.status}`}>
      <header className="mcp-approval-head">
        <strong>{approval.title}</strong>
        <span className={`mcp-approval-status is-${approval.status}`}>
          {STATUS_LABELS[approval.status]}
        </span>
      </header>
      <div className="mcp-approval-meta">
        <span>{preview.operation ?? approval.tool}</span>
        <span>{amountLine(preview)}</span>
        <span>{formatTime(approval.createdAt)}</span>
      </div>

      {paths.length > 0 ? (
        <ul className="mcp-approval-sample mcp-approval-paths">
          {paths.map((path, index) => (
            <li key={`${path}-${index}`}>
              <span title={path}>{path}</span>
            </li>
          ))}
          <Remaining
            total={preview.pathTotal ?? preview.paths?.length ?? 0}
            shown={paths.length}
            label="个路径未展开"
          />
        </ul>
      ) : null}

      {meta.length > 0 ? (
        <div className="mcp-approval-meta">
          {meta.map((item) => (
            <span key={item}>{item}</span>
          ))}
        </div>
      ) : null}

      {sample.length > 0 ? (
        <ul className="mcp-approval-sample">
          {sample.map((item) => (
            <li key={item.id}>
              <code>#{item.id}</code>
              <span>{item.folderPath ?? "—"}</span>
              {typeof item.score === "number" ? <em>{item.score} 星</em> : null}
            </li>
          ))}
          <Remaining
            total={preview.sample?.length ?? 0}
            shown={sample.length}
            label="张未展开"
          />
        </ul>
      ) : null}

      {entries.length > 0 ? (
        <ul className="mcp-approval-sample">
          {entries.map((entry) => (
            <li key={entry.id}>
              <code>#{entry.id}</code>
              <span>{entry.label}</span>
              <em>{entry.affected.toLocaleString()} 张</em>
            </li>
          ))}
          <Remaining
            total={preview.entries?.length ?? 0}
            shown={entries.length}
            label="条记录"
          />
        </ul>
      ) : null}

      {consequences.length > 0 ? (
        <ul className="mcp-approval-consequences">
          {consequences.map((item) => (
            <li key={item}>{item}</li>
          ))}
        </ul>
      ) : null}

      {preview.blockedReason ? (
        <div className="mcp-approval-error">{preview.blockedReason}</div>
      ) : null}
      {approval.error ? <div className="mcp-approval-error">{approval.error}</div> : null}
      {approval.taskId ? (
        <div className="mcp-approval-task">任务 {approval.taskId}</div>
      ) : null}

      {open ? (
        <div className="mcp-approval-actions">
          <Button
            size="sm"
            variant="primary"
            icon={<ShieldCheck size={13} />}
            disabled={busy}
            onClick={() => onApprove(approval)}
          >
            批准
          </Button>
          <Button
            size="sm"
            variant="ghost"
            icon={<ShieldX size={13} />}
            disabled={busy}
            onClick={() => onReject(approval)}
          >
            拒绝
          </Button>
        </div>
      ) : null}
    </article>
  );
}

/** 批准/拒绝共用一套调用与提示；`onSettled` 由调用方决定刷新哪些列表。 */
export function useApprovalActions(onSettled: () => void) {
  const { notify } = useNotices();
  const [busyId, setBusyId] = useState<string | null>(null);

  const run = useCallback(
    async (approval: McpApproval, approve: boolean) => {
      setBusyId(approval.id);
      try {
        if (approve) {
          await appApi.approveMcpApproval(approval.id);
          notify("已批准 AI 操作", "success");
        } else {
          await appApi.rejectMcpApproval(approval.id);
          notify("已拒绝 AI 操作", "info");
        }
      } catch (cause) {
        notify(
          approve ? "批准 AI 操作失败" : "拒绝 AI 操作失败",
          "error",
          errorMessage(cause),
        );
      } finally {
        setBusyId(null);
        onSettled();
      }
    },
    [notify, onSettled],
  );

  const approve = useCallback(
    (approval: McpApproval) => void run(approval, true),
    [run],
  );
  const reject = useCallback(
    (approval: McpApproval) => void run(approval, false),
    [run],
  );

  return { busyId, approve, reject };
}

export function useMcpApprovals(enabled: boolean) {
  const [approvals, setApprovals] = useState<McpApproval[]>([]);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    if (!enabled || browserPreview) {
      setApprovals([]);
      return;
    }
    try {
      setApprovals(await appApi.listMcpApprovals());
      setError(null);
    } catch (cause) {
      setError(errorMessage(cause));
    }
  }, [enabled]);

  useEffect(() => {
    if (!enabled) {
      setApprovals([]);
      return;
    }
    void refresh();
    if (browserPreview) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    const timer = window.setInterval(() => void refresh(), 30_000);
    void (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      if (disposed) return;
      const changed = await listen("mcp-approval-changed", () => void refresh());
      // 确认单绑定的移动/撤销任务结束后，状态要立刻从“执行中”落下。
      const finished = await listen("task-finished", () => void refresh());
      unlisten = () => {
        changed();
        finished();
      };
    })();
    return () => {
      disposed = true;
      window.clearInterval(timer);
      unlisten?.();
    };
  }, [enabled, refresh]);

  return { approvals, error, refresh };
}

/** 非模态确认卡片：停靠在任务中心上方，不阻塞浏览。 */
export function McpApprovalHost({ enabled }: { enabled: boolean }) {
  const { notify } = useNotices();
  const { approvals, refresh } = useMcpApprovals(enabled);
  const { busyId, approve, reject } = useApprovalActions(refresh);
  const previousPending = useRef(0);

  const pending = useMemo(
    () =>
      approvals.filter(
        (approval) => approval.status === "pending" && approval.expiresAt > Date.now(),
      ),
    [approvals],
  );
  const running = useMemo(
    () => approvals.filter((approval) => approval.status === "running"),
    [approvals],
  );
  const visible = [...pending, ...running].slice(0, 3);

  useEffect(() => {
    if (pending.length > previousPending.current && pending[0]) {
      notify("AI 请求确认", "info", pending[0].title);
    }
    previousPending.current = pending.length;
  }, [pending, notify]);

  if (!enabled || browserPreview || visible.length === 0) return null;

  return (
    <div className="mcp-approval-host">
      {visible.map((approval) => (
        <ApprovalCard
          key={approval.id}
          approval={approval}
          busy={busyId !== null}
          onApprove={approve}
          onReject={reject}
        />
      ))}
    </div>
  );
}
