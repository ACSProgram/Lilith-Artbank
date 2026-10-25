import { useCallback, useEffect, useMemo, useState, type RefObject } from "react";
import { History, Trash2, X } from "lucide-react";
import { useNotices } from "../../app/noticeContext";
import { useTaskCenter } from "../../app/taskContext";
import type { TaskStartResult } from "../../app/taskTypes";
import { Button, IconButton } from "../../shared/components/Button";
import { Popover } from "../../shared/components/Popover";
import { Spinner } from "../../shared/components/Spinner";
import { confirmClearHistory, confirmUndoTo } from "../../shared/confirm";
import { errorMessage } from "../../shared/errors";
import { formatBytes } from "../../shared/format";
import { libraryApi } from "./api";
import type { HistoryPage } from "./types";

interface OperationHistoryPanelProps {
  open: boolean;
  anchorRef: RefObject<HTMLButtonElement | null>;
  onClose: () => void;
  onChanged: () => void;
}

function formatTime(value: number): string {
  return new Date(value).toLocaleString(undefined, {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function OperationHistoryPanel({
  open,
  anchorRef,
  onClose,
  onChanged,
}: OperationHistoryPanelProps) {
  const { notify, confirm } = useNotices();
  const { active, dataVersion } = useTaskCenter();
  const [page, setPage] = useState<HistoryPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [selectedId, setSelectedId] = useState<number | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const next = await libraryApi.getOperationHistory();
      setPage(next);
      setSelectedId((current) =>
        current != null && next.entries.some((entry) => entry.id === current)
          ? current
          : null,
      );
    } catch (cause) {
      notify("读取操作记录失败", "error", errorMessage(cause));
    } finally {
      setLoading(false);
    }
  }, [notify]);

  useEffect(() => {
    if (open) void load();
  }, [open, dataVersion, load]);

  const undoTask = active.find((task) => task.kind === "undo") ?? null;
  const selectedIndex =
    page?.entries.findIndex((entry) => entry.id === selectedId) ?? -1;
  const selection = useMemo(() => {
    if (!page || selectedIndex < 0) return null;
    const chain = page.entries.slice(0, selectedIndex + 1);
    return {
      target: page.entries[selectedIndex],
      count: chain.length,
      affected: chain.reduce((sum, entry) => sum + entry.affected, 0),
      failed: chain.find((entry) => entry.status === "undoFailed") ?? null,
    };
  }, [page, selectedIndex]);

  const undoDisabled =
    !page?.canUndo ||
    busy ||
    undoTask != null ||
    !selection ||
    selection.target.status !== "applied" ||
    selection.failed != null;

  const undoReason =
    page?.blockedReason ??
    (selection?.failed
      ? "有一条记录上次撤销失败，需要先处理它"
      : selection && selection.target.status !== "applied"
        ? "这条记录不能作为撤销目标"
        : null);

  const handleResult = (
    result: TaskStartResult,
    successText: string,
    conflictTitle: string,
  ) => {
    if (result.status === "conflict") {
      const label = result.active[0]?.label ?? "其他任务";
      notify(conflictTitle, "error", `「${label}」正在进行，请等它完成后重试。`);
      return false;
    }
    notify(successText, "success");
    return true;
  };

  const undo = async () => {
    if (!page || !selection || undoDisabled) return;
    const accepted = await confirmUndoTo(
      confirm,
      selection.target.label,
      selection.count,
      selection.affected,
    );
    if (!accepted) return;
    setBusy(true);
    try {
      const result = await libraryApi.undoToOperation(selection.target.id);
      if (handleResult(result, "已开始撤销", "现在不能撤销")) {
        setSelectedId(null);
        onChanged();
      }
    } catch (cause) {
      notify("撤销失败", "error", errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  const clear = async () => {
    if (!page || busy) return;
    const accepted = await confirmClearHistory(
      confirm,
      page.pendingPurgeCount,
      page.pendingPurgeBytes,
    );
    if (!accepted) return;
    setBusy(true);
    try {
      const result = await libraryApi.clearOperationHistory();
      if (handleResult(result, "已开始清空操作记录", "现在不能清空记录")) onChanged();
    } catch (cause) {
      notify("清空操作记录失败", "error", errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Popover
      open={open}
      anchorRef={anchorRef}
      placement="bottom-end"
      minWidth={360}
      className="operation-history-popover"
      onClose={onClose}
      closeOnScroll={false}
    >
      <div className="operation-history">
        <header className="operation-history-header">
          <span className="operation-history-title">
            <History size={14} />
            操作记录
          </span>
          <IconButton label="关闭操作记录" onClick={onClose}>
            <X size={13} />
          </IconButton>
        </header>

        {undoTask ? (
          <div className="operation-history-progress">
            <Spinner size={13} />
            <span>
              {undoTask.message || "正在撤销"} {undoTask.completed}/{undoTask.total}
            </span>
          </div>
        ) : null}

        <div className="operation-history-list">
          {loading && !page ? (
            <div className="operation-history-empty">
              <Spinner size={14} /> 正在读取…
            </div>
          ) : page && page.entries.length > 0 ? (
            page.entries.map((entry) => (
              <button
                key={entry.id}
                type="button"
                className={`operation-history-row${selectedId === entry.id ? " is-selected" : ""}`}
                aria-pressed={selectedId === entry.id}
                onClick={() =>
                  setSelectedId((current) => (current === entry.id ? null : entry.id))
                }
              >
                <span className="operation-history-label">{entry.label}</span>
                <span className="operation-history-meta">
                  {formatTime(entry.createdAt)}
                  {entry.affected > 0 ? ` · ${entry.affected.toLocaleString()} 张` : ""}
                </span>
                {entry.status === "undoFailed" ? (
                  <span className="operation-history-status">
                    撤销失败{entry.error ? `：${entry.error}` : ""}
                  </span>
                ) : null}
              </button>
            ))
          ) : (
            <div className="operation-history-empty">
              {page?.blockedReason ?? "还没有操作记录"}
            </div>
          )}
        </div>

        {page && page.entries.length > 0 ? (
          <div className="operation-history-actions">
            <Button
              size="sm"
              variant="secondary"
              disabled={undoDisabled}
              loading={busy && undoTask == null}
              title={undoReason ?? undefined}
              onClick={() => void undo()}
            >
              撤销这条及之后
            </Button>
            <span className="operation-history-hint">
              {selection
                ? `共 ${selection.count.toLocaleString()} 条记录，影响 ${selection.affected.toLocaleString()} 张图片`
                : "选择一条记录后撤销"}
            </span>
          </div>
        ) : null}

        <footer className="operation-history-footer">
          <span>
            {page && page.pendingPurgeCount > 0
              ? `待释放 ${page.pendingPurgeCount.toLocaleString()} 张 · ${formatBytes(page.pendingPurgeBytes)}`
              : "没有待释放的文件"}
            {page ? ` · 保留最近 ${page.maxSteps} 步` : ""}
          </span>
          <Button
            size="sm"
            variant="danger"
            icon={<Trash2 size={13} />}
            disabled={busy || undoTask != null || !page || page.entries.length === 0}
            onClick={() => void clear()}
          >
            清空记录
          </Button>
        </footer>
      </div>
    </Popover>
  );
}
