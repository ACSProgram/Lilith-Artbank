import type { LibraryQuery } from "../modules/library/types";
import type { BatchTaskContextInfo } from "./batchTaskContext";
import type { ActionAvailability } from "./capability";

export type ScopeChoice =
  | "selected"
  | "folder"
  | "collection"
  | "view"
  | "temp"
  | "library";

/** 前端只传语义范围；id 集合由 Rust 侧解析。 */
export type TaskScope =
  | { type: "selected"; imageIds: number[] }
  | { type: "folder"; folderId: number; includeSubfolders: boolean }
  | { type: "collection"; collectionId: number }
  | { type: "view"; query: LibraryQuery }
  | { type: "temp" }
  | { type: "library" };

export const SCOPE_ORDER: ScopeChoice[] = [
  "selected",
  "folder",
  "collection",
  "view",
  "temp",
  "library",
];

export interface ScopeBuildOptions {
  /** 文件夹范围是否包含后代；批量任务固定包含。 */
  includeSubfolders?: boolean;
}

export function buildScope(
  choice: ScopeChoice,
  context: BatchTaskContextInfo,
  options?: ScopeBuildOptions,
): TaskScope | null {
  switch (choice) {
    case "selected":
      return context.selectedIds.length > 0
        ? { type: "selected", imageIds: context.selectedIds }
        : null;
    case "folder":
      return context.folderId != null
        ? {
            type: "folder",
            folderId: context.folderId,
            includeSubfolders: options?.includeSubfolders ?? true,
          }
        : null;
    case "collection":
      return context.collectionId != null
        ? { type: "collection", collectionId: context.collectionId }
        : null;
    case "view":
      return context.query ? { type: "view", query: context.query } : null;
    case "temp":
      return { type: "temp" };
    case "library":
      return { type: "library" };
  }
}

/** 默认范围：优先当前选择，其次当前视图，最后全库；找相似可以从“当前视图”起。 */
export function defaultScopeChoice(
  context: BatchTaskContextInfo | null,
  preferred: ScopeChoice = "selected",
): ScopeChoice {
  const order: ScopeChoice[] =
    preferred === "view" ? ["view", "selected"] : ["selected", "view"];
  for (const choice of order) {
    if (choice === "selected" && context && context.selectedIds.length > 0) return "selected";
    if (choice === "view" && context?.query) return "view";
  }
  return "library";
}

export function scopeName(
  choice: ScopeChoice,
  context: BatchTaskContextInfo | null,
): string {
  switch (choice) {
    case "selected":
      return "当前选择";
    case "folder":
      return context?.folderName || "当前文件夹";
    case "collection":
      return context?.collectionName || "当前集合";
    case "view":
      return "当前视图";
    case "temp":
      return "临时文件夹";
    case "library":
      return "整个图库";
  }
}

/** 范围当前可显示的图片数；拿不到时返回 null。 */
export function scopeCountValue(
  choice: ScopeChoice,
  context: BatchTaskContextInfo | null,
  options?: ScopeBuildOptions,
): number | null {
  if (!context) return null;
  switch (choice) {
    case "selected":
      return context.selectedIds.length;
    case "folder": {
      const includeSubfolders = options?.includeSubfolders ?? true;
      return includeSubfolders ? context.folderTotal : context.folderDirectTotal;
    }
    case "collection":
      return context.collectionTotal;
    case "view":
      return context.viewTotal;
    case "temp":
      return context.tempTotal;
    case "library":
      return context.libraryTotal;
  }
}

export function scopeCountLabel(
  choice: ScopeChoice,
  context: BatchTaskContextInfo | null,
  options?: ScopeBuildOptions,
): string {
  const total = scopeCountValue(choice, context, options);
  if (total == null) return "";
  if (choice === "folder") {
    const includeSubfolders = options?.includeSubfolders ?? true;
    return `${total.toLocaleString()} 张${includeSubfolders ? "（含子文件夹）" : ""}`;
  }
  return `${total.toLocaleString()} 张`;
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
        : { enabled: false, reason: "当前视图不能作为范围" },
    temp:
      context && context.tempTotal > 0
        ? { enabled: true }
        : { enabled: false, reason: "临时文件夹没有图片" },
    library: { enabled: true },
  };
}
