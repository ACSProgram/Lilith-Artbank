import type { BatchTaskContextInfo } from "../shared/batchTaskContext";
import type { ActionAvailability } from "../shared/capability";
import type { TaskScope } from "./taskTypes";

export type ScopeChoice = "selected" | "folder" | "collection" | "view" | "library";

export function buildScope(
  choice: ScopeChoice,
  context: BatchTaskContextInfo,
): TaskScope | null {
  switch (choice) {
    case "selected":
      return context.selectedIds.length > 0
        ? { type: "selected", imageIds: context.selectedIds }
        : null;
    case "folder":
      return context.folderId != null
        ? { type: "folder", folderId: context.folderId, includeSubfolders: true }
        : null;
    case "collection":
      return context.collectionId != null
        ? { type: "collection", collectionId: context.collectionId }
        : null;
    case "view":
      return context.query ? { type: "view", query: context.query } : null;
    case "library":
      return { type: "library" };
  }
}

export function defaultScopeChoice(context: BatchTaskContextInfo | null): ScopeChoice {
  if (context && context.selectedIds.length > 0) return "selected";
  return context?.query ? "view" : "library";
}

export function scopeCountLabel(
  choice: ScopeChoice,
  context: BatchTaskContextInfo | null,
): string {
  if (!context) return "";
  switch (choice) {
    case "selected":
      return `${context.selectedIds.length.toLocaleString()} 张`;
    case "folder":
      return context.folderTotal != null
        ? `${context.folderTotal.toLocaleString()} 张（含子文件夹）`
        : "";
    case "collection":
      return context.collectionTotal != null
        ? `${context.collectionTotal.toLocaleString()} 张`
        : "";
    case "view":
      return context.viewTotal != null ? `${context.viewTotal.toLocaleString()} 张` : "";
    case "library":
      return `${context.libraryTotal.toLocaleString()} 张`;
  }
}

export function scopeAvailability(
  context: BatchTaskContextInfo | null,
): Record<ScopeChoice, ActionAvailability> {
  return {
    selected:
      context && context.selectedIds.length > 0
        ? { enabled: true }
        : { enabled: false, reason: "还没有选择图片" },
    folder:
      context?.folderId != null
        ? { enabled: true }
        : { enabled: false, reason: "请在侧栏选择单个文件夹" },
    collection:
      context?.collectionId != null
        ? { enabled: true }
        : { enabled: false, reason: "当前不在集合视图" },
    view:
      context?.query != null
        ? { enabled: true }
        : { enabled: false, reason: "相似结果不能作为任务范围" },
    library: { enabled: true },
  };
}
