import type { ConfirmOptions } from "../app/noticeContext";
import { formatBytes } from "./format";

/** `useNotices().confirm` 的签名；语义化封装只依赖这个函数，方便在任意组件里调用。 */
export type ConfirmFn = (options: ConfirmOptions) => Promise<boolean>;

function count(value: number): string {
  return Math.max(0, Math.trunc(value)).toLocaleString();
}

/**
 * 移入回收站：可恢复，所以不标红，但必须确认（四个入口共用这一个收口点）。
 */
export function confirmTrash(confirm: ConfirmFn, imageCount: number): Promise<boolean> {
  return confirm({
    title: `把 ${count(imageCount)} 张图片移入回收站？`,
    message: "可以随时从回收站恢复；永久删除才会删除原图文件。",
    confirmLabel: "移入回收站",
  });
}

/** 永久删除：先移出回收站；文件延迟释放，清空操作记录后不可撤销。 */
export function confirmDelete(confirm: ConfirmFn, imageCount: number): Promise<boolean> {
  return confirm({
    title: `永久删除 ${count(imageCount)} 张图片？`,
    message: "会从回收站移除；原图文件要等这条记录从操作记录中清除后才释放。",
    detail: "清除操作记录后不能再撤销。",
    confirmLabel: "永久删除",
    danger: true,
  });
}

/** 清空回收站：同样延迟释放，文件不会立刻消失。 */
export function confirmEmptyTrash(confirm: ConfirmFn, imageCount: number): Promise<boolean> {
  return confirm({
    title: "清空回收站？",
    message: `会把回收站中的 ${count(imageCount)} 张图片移出回收站；原图文件要等这些记录从操作记录中清除后才释放。`,
    detail: "清除操作记录后不能再撤销。",
    confirmLabel: "清空回收站",
    danger: true,
  });
}

/** 线性回退：目标记录及其之后的全部记录都会撤销。 */
export function confirmUndoTo(
  confirm: ConfirmFn,
  targetLabel: string,
  recordCount: number,
  affectedCount: number,
): Promise<boolean> {
  return confirm({
    title: `撤销到「${targetLabel}」之前？`,
    message: `会撤销这条及其之后的 ${count(recordCount)} 条记录，共影响 ${count(affectedCount)} 张图片。`,
    detail: "撤销后这些记录会从列表移除。",
    confirmLabel: "撤销",
  });
}

/** 清空操作记录：立即释放待删除文件，并且之后不能再撤销。 */
export function confirmClearHistory(
  confirm: ConfirmFn,
  pendingCount: number,
  pendingBytes: number,
): Promise<boolean> {
  return confirm({
    title: "清空操作记录？",
    message: "所有记录都会被删除，图库不能再撤销到这些操作之前。",
    detail:
      pendingCount > 0
        ? `已从回收站移除的 ${count(pendingCount)} 张图片会立即释放磁盘（约 ${formatBytes(pendingBytes)}）。`
        : "当前没有待释放的文件。",
    confirmLabel: "清空记录",
    danger: true,
  });
}
