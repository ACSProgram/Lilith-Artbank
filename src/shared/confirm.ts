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

/** 线性重做：从当前状态到目标记录之间的灰记录会重新应用。 */
export function confirmRedoTo(
  confirm: ConfirmFn,
  targetLabel: string,
  recordCount: number,
  affectedCount: number,
  unit = "张",
): Promise<boolean> {
  return confirm({
    title: `重做到「${targetLabel}」？`,
    message: `会重新应用当前状态到这条记录之间的 ${count(recordCount)} 条记录，共影响 ${count(affectedCount)} ${unit}。`,
    detail: "重做后这些记录会重新变为可撤销。",
    confirmLabel: "重做",
  });
}

/** 线性回退：目标记录及其上方仍为 applied 的记录都会撤销。 */
export function confirmUndoTo(
  confirm: ConfirmFn,
  targetLabel: string,
  recordCount: number,
  affectedCount: number,
  unit = "张",
): Promise<boolean> {
  return confirm({
    title: `撤销到「${targetLabel}」之前？`,
    message: `会从当前状态回退到这条记录之前，撤销 ${count(recordCount)} 条尚未撤销的记录，共影响 ${count(affectedCount)} ${unit}。`,
    detail: "这些记录会变灰，之后可以重做。",
    confirmLabel: "撤销",
  });
}

/** 清空操作记录：立即释放待删除文件，并且之后不能再撤销。 */
export function confirmClearHistory(
  confirm: ConfirmFn,
  pendingCount: number,
  pendingBytes: number,
  storageBytes: number,
): Promise<boolean> {
  const storage =
    storageBytes > 0
      ? `会删除约 ${formatBytes(storageBytes)} 撤销数据；数据库文件不会立即缩小。`
      : "";
  const pending =
    pendingCount > 0
      ? `已从回收站移除的 ${count(pendingCount)} 张图片会立即释放磁盘（约 ${formatBytes(pendingBytes)}）。`
      : "";
  return confirm({
    title: "清空操作记录？",
    message: "所有记录都会被删除，图库不能再撤销到这些操作之前。",
    detail: [storage, pending].filter(Boolean).join(" "),
    confirmLabel: "清空记录",
    danger: true,
  });
}

/** 导入时压缩：原图不落盘；压不动的图仍保留原图。 */
export function confirmImportOptimize(
  confirm: ConfirmFn,
  pathCount: number,
): Promise<boolean> {
  return confirm({
    title: "导入时压缩这些图片？",
    message: `会压缩本次从 ${count(pathCount)} 个路径导入的图片；压不动的图保留原图。`,
    detail: "压缩后的原图不会保留，无法还原。",
    confirmLabel: "导入并压缩",
    danger: true,
  });
}

/** 空间优化执行：替换原图且有损；执行中可以取消，但已完成的图片不会恢复。 */
export function confirmOptimize(
  confirm: ConfirmFn,
  processCount: number,
  savingsBytes: number,
): Promise<boolean> {
  return confirm({
    title: "开始压缩这些图片？",
    message: `会用压缩后的版本替换约 ${count(processCount)} 张图片的原图${
      savingsBytes > 0 ? `，预计省 ${formatBytes(savingsBytes)}` : ""
    }。`,
    detail:
      "压缩会损失一点画质，原图无法恢复；执行中可以随时取消，已完成的图片保持压缩后的版本。",
    confirmLabel: "开始压缩",
    danger: true,
  });
}
