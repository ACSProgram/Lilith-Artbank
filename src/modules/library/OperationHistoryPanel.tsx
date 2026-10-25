import {
  Fragment,
  useCallback,
  useEffect,
  useMemo,
  useState,
  type RefObject,
} from "react";
import { History, Trash2, X } from "lucide-react";
import { useNotices } from "../../app/noticeContext";
import { useTaskCenter } from "../../app/taskContext";
import type { TaskStartResult } from "../../app/taskTypes";
import { Button, IconButton } from "../../shared/components/Button";
import { Popover } from "../../shared/components/Popover";
import { Spinner } from "../../shared/components/Spinner";
import {
  confirmClearHistory,
  confirmRedoTo,
  confirmUndoTo,
} from "../../shared/confirm";
import { errorMessage } from "../../shared/errors";
import { formatBytes } from "../../shared/format";
import { libraryApi } from "./api";
import type { HistoryEntry, HistoryPage } from "./types";

// 文件夹、集合结构与标签改名的记录按“项”计，其余（导入、移动、图片元数据）按图片计。
const ITEM_KINDS = new Set([
  "collectionCreate",
  "collectionRename",
  "collectionReorder",
  "tagRename",
]);

function affectedUnit(kinds: string[]): string {
  return kinds.every(
    (kind) => kind.startsWith("folder") || ITEM_KINDS.has(kind),
  )
    ? "项"
    : "张";
}

function isRedoStatus(entry: HistoryEntry): boolean {
  return entry.status === "undone" || entry.status === "redoing";
}

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

  const historyTask =
    active.find((task) => task.kind === "undo" || task.kind === "redo") ?? null;
  const selectedIndex =
    page?.entries.findIndex((entry) => entry.id === selectedId) ?? -1;
  // 列表按新到旧排列；最上面连续的 undone/redoing 就是可重做区，分隔线是当前状态。
  const boundary = useMemo(() => {
    if (!page) return 0;
    const index = page.entries.findIndex((entry) => !isRedoStatus(entry));
    return index < 0 ? page.entries.length : index;
  }, [page]);

  const selection = useMemo(() => {
    if (!page || selectedIndex < 0) return null;
    const target = page.entries[selectedIndex];
    if (isRedoStatus(target)) {
      const chain = page.entries.slice(selectedIndex, boundary);
      return {
        mode: "redo" as const,
        target,
        count: chain.length,
        affected: chain.reduce((sum, entry) => sum + entry.affected, 0),
        unit: affectedUnit(chain.map((entry) => entry.kind)),
        failed: null,
        blocked: null,
      };
    }
    if (target.status === "applied") {
      const chain = page.entries.slice(0, selectedIndex + 1);
      // 已经变灰的记录不会再次撤销；真正回退的是这条及其上方仍为 applied 的记录。
      const active = chain.filter((entry) => entry.status === "applied");
      return {
        mode: "undo" as const,
        target,
        count: active.length,
        affected: active.reduce((sum, entry) => sum + entry.affected, 0),
        unit: affectedUnit(active.map((entry) => entry.kind)),
        failed:
          chain.find(
            (entry) =>
              entry.status === "undoFailed" || (isRedoStatus(entry) && entry.error),
          ) ?? null,
        blocked: active.some((entry) => entry.kind === "optimize")
          ? "压缩记录不可撤销，原图已删除"
          : null,
      };
    }
    return null;
  }, [page, selectedIndex, boundary]);

  const actionDisabled =
    !page?.canRun ||
    busy ||
    historyTask != null ||
    !selection ||
    selection.blocked != null ||
    (selection.mode === "undo" &&
      (selection.target.status !== "applied" || selection.failed != null));

  const actionReason =
    page?.blockedReason ??
    selection?.blocked ??
    (selection?.failed
      ? selection.failed.status === "undoFailed"
        ? "有一条记录上次撤销失败，需要先处理它"
        : "有一条记录上次重做失败，需要先重试它"
      : selection?.mode === "undo" && selection.target.status !== "applied"
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

  const runAction = async () => {
    if (!page || !selection || actionDisabled) return;
    const redo = selection.mode === "redo";
    const accepted = redo
      ? await confirmRedoTo(
          confirm,
          selection.target.label,
          selection.count,
          selection.affected,
          selection.unit,
        )
      : await confirmUndoTo(
          confirm,
          selection.target.label,
          selection.count,
          selection.affected,
          selection.unit,
        );
    if (!accepted) return;
    setBusy(true);
    try {
      const result = redo
        ? await libraryApi.redoToOperation(selection.target.id)
        : await libraryApi.undoToOperation(selection.target.id);
      if (
        handleResult(
          result,
          redo ? "已开始重做" : "已开始撤销",
          redo ? "现在不能重做" : "现在不能撤销",
        )
      ) {
        setSelectedId(null);
        onChanged();
      }
    } catch (cause) {
      notify(redo ? "重做失败" : "撤销失败", "error", errorMessage(cause));
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

        {historyTask ? (
          <div className="operation-history-progress">
            <Spinner size={13} />
            <span>
              {historyTask.message ||
                (historyTask.kind === "redo" ? "正在重做" : "正在撤销")}{" "}
              {historyTask.completed}/{historyTask.total}
            </span>
          </div>
        ) : null}

        <div className="operation-history-list">
          {loading && !page ? (
            <div className="operation-history-empty">
              <Spinner size={14} /> 正在读取…
            </div>
          ) : page && page.entries.length > 0 ? (
            <>
              {page.entries.map((entry, index) => (
                <Fragment key={entry.id}>
                  {index === boundary ? (
                    <div className="operation-history-current" role="separator">
                      <span>当前状态</span>
                    </div>
                  ) : null}
                  <button
                    type="button"
                    className={`operation-history-row${
                      selectedId === entry.id ? " is-selected" : ""
                    }${isRedoStatus(entry) ? " is-undone" : ""}`}
                    aria-pressed={selectedId === entry.id}
                    onClick={() =>
                      setSelectedId((current) =>
                        current === entry.id ? null : entry.id,
                      )
                    }
                  >
                    <span className="operation-history-label">{entry.label}</span>
                    <span className="operation-history-meta">
                      {formatTime(entry.createdAt)}
                      {entry.affected > 0
                        ? ` · ${entry.affected.toLocaleString()} ${affectedUnit([entry.kind])}`
                        : ""}
                    </span>
                    {entry.status === "undoFailed" ? (
                      <span className="operation-history-status">
                        撤销失败{entry.error ? `：${entry.error}` : ""}
                      </span>
                    ) : isRedoStatus(entry) && entry.error ? (
                      <span className="operation-history-status">
                        上次重做失败：{entry.error}
                      </span>
                    ) : null}
                  </button>
                </Fragment>
              ))}
              {boundary === page.entries.length ? (
                <div className="operation-history-current" role="separator">
                  <span>当前状态</span>
                </div>
              ) : null}
            </>
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
              disabled={actionDisabled}
              loading={busy && historyTask == null}
              title={actionReason ?? undefined}
              onClick={() => void runAction()}
            >
              {selection?.mode === "redo" ? "重做到这条" : "撤销到这条之前"}
            </Button>
            <span className="operation-history-hint">
              {selection
                ? selection.mode === "redo"
                  ? `将重做 ${selection.count.toLocaleString()} 条记录，影响 ${selection.affected.toLocaleString()} ${selection.unit}`
                  : `共 ${selection.count.toLocaleString()} 条记录，影响 ${selection.affected.toLocaleString()} ${selection.unit}`
                : "选择一条记录后撤销或重做"}
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
            disabled={busy || historyTask != null || !page || page.entries.length === 0}
            onClick={() => void clear()}
          >
            清空记录
          </Button>
        </footer>
      </div>
    </Popover>
  );
}
