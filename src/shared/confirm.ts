import type { ConfirmOptions } from "../app/noticeContext";

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

/** 永久删除：删记录 + 删原图文件，不可恢复。 */
export function confirmDelete(confirm: ConfirmFn, imageCount: number): Promise<boolean> {
  return confirm({
    title: `永久删除 ${count(imageCount)} 张图片？`,
    message: "会从图库删除记录与原图文件，无法恢复。",
    detail: "标签、集合引用与缩略图指针会一并清理。",
    confirmLabel: "永久删除",
    danger: true,
  });
}

/** 清空回收站：一次性永久删除回收站里的全部图片。 */
export function confirmEmptyTrash(confirm: ConfirmFn, imageCount: number): Promise<boolean> {
  return confirm({
    title: "清空回收站？",
    message: `会永久删除回收站中的 ${count(imageCount)} 张图片及其原图文件，无法恢复。`,
    confirmLabel: "清空回收站",
    danger: true,
  });
}
