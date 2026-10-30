import type { ImageRow } from "./library";

export type ChangeKind =
  | "metadata"
  | "folder"
  | "membership"
  | "collection"
  | "tagCatalog"
  | "features"
  | "thumbnails"
  | "content"
  | "unknown"
  | "none";

export type ChangeSource = "local" | "task" | "mcp" | "import";
export type ChangeField = "score" | "tags" | "description" | "folder" | "collection";
export type FeatureKind = "description" | "embedding" | "pose";
export type TagChangeAction = "add" | "remove" | "set" | "rename" | "delete" | "merge";

export interface RowPatch {
  id: number;
  update: Partial<ImageRow>;
}

/** 本地写入已知的图库计数变化；有值时不再做全库计数查询。 */
export interface LibraryCountDelta {
  images?: number;
  trash?: number;
  temp?: number;
}

/** 一次写入引起的图库变化；由 app 层构造，LibraryView 只负责执行。 */
export interface DataChange {
  kind: ChangeKind;
  source: ChangeSource;
  /** TaskKind、MCP 工具名或本地操作名；用于诊断与保守回退。 */
  reason?: string;
  /** 已知受影响的图片；外部任务/MCP 通常拿不到。 */
  ids?: number[];
  fields?: ChangeField[];
  tagAction?: TagChangeAction;
  tagId?: number;
  /** 合并时保留的目标标签。 */
  targetTagId?: number;
  score?: number | null;
  /** 描述是否为空；外部写入只回传这个判断，不回传整段描述。 */
  descriptionEmpty?: boolean;
  targetFolderPath?: string;
  feature?: FeatureKind;
  rowPatches?: RowPatch[];
  /** 外部范围变化：调用方确认选择已经无法可靠保留。 */
  clearSelection?: boolean;
  /** 已知的图片/回收站/临时区计数变化；缺一条就回退到重新计数。 */
  countDelta?: LibraryCountDelta;
  /** 文件夹列表已由调用方更新，无需再次读取。 */
  foldersAlreadyLoaded?: boolean;
  similarity: "keep" | "clear";
}

export interface DataChangeSignal {
  revision: number;
  changes: DataChange[];
}

export type ListAction = "none" | "patch" | "refresh";
export type SelectionAction = "keep" | "prune" | "clear";

/**
 * 一次变化对各处界面状态的效果。`selection` 的 `prune` 由 `selectionAfterChange` 落实，
 * `clear` 表示连活动图与锚点一起清空。查看器是否复核活动图由 `ViewerHost` 自己按写入版本
 * 与外部变化代次决定，不在这里表达。
 */
export interface DataChangePlan {
  list: ListAction;
  selection: SelectionAction;
  folders: boolean;
  collections: boolean;
  status: boolean;
  features: boolean;
  facets: boolean;
  tagOptions: boolean;
  detail: boolean;
  similarity: "keep" | "clear";
  /** 已经不存在的标签 id（删除或合并的源）：筛选条件里带着它们会让列表永远为空。 */
  dropTags: number[];
}

export interface PlanContext {
  hasMore: boolean;
}
