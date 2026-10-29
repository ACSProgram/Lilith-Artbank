import type {
  ImageRow,
  LibraryFilters,
  LibraryQuery,
  LibraryView,
  SearchScope,
  SortKey,
  SortSpec,
} from "./types";
import type { FindSimilarConfig, SimilarKind } from "../../shared/similarity";
import type { TagFilterAction, TagFilterScope } from "../../shared/tagFilter";

/** 与 Rust `query::DEFAULT_LIMIT` 保持一致。 */
export const PAGE_SIZE = 150;

export const DEFAULT_SORT: SortSpec = { key: "importedAt", descending: true };
export const DEFAULT_SORT_LEVELS: SortSpec[] = [DEFAULT_SORT];
export const COLLECTION_SORT: SortSpec = { key: "collectionPosition", descending: false };
export const COLLECTION_SORT_LEVELS: SortSpec[] = [COLLECTION_SORT];

export const SORT_KEYS: SortKey[] = [
  "importedAt",
  "modifiedAt",
  "score",
  "size",
  "pixels",
  "folder",
  "collectionPosition",
];

export const SORT_LABELS: Record<SortKey, string> = {
  importedAt: "导入时间",
  modifiedAt: "修改时间",
  score: "评分",
  size: "文件大小",
  pixels: "像素尺寸",
  folder: "文件夹",
  collectionPosition: "集合内顺序",
};

/**
 * 合并相似组时能用来决定保留哪张的指标。
 *
 * 「集合内顺序」不在其中：合并发生在跨集合的范围上，按集合位置保留没有稳定含义。
 */
export const KEEP_SORT_KEYS: SortKey[] = [
  "score",
  "importedAt",
  "modifiedAt",
  "pixels",
  "size",
  "folder",
];

/** 相似分组合并的默认保留规则：评分高的先留，其次导入时间晚的先留。 */
export const DEFAULT_KEEP_SORT: SortSpec[] = [
  { key: "score", descending: true },
  { key: "importedAt", descending: true },
];

/** 保留规则的一句话说明；确认框与按钮提示共用，空规则按默认值说。 */
export function keepSortSummary(sort: SortSpec[]): string {
  const levels = sort.length > 0 ? sort : DEFAULT_KEEP_SORT;
  return levels
    .map((spec) => `${SORT_LABELS[spec.key]}${spec.descending ? "降序" : "升序"}`)
    .join(" → ");
}

export interface BrowseState {
  view: LibraryView;
  collectionId: number | null;
  search: string;
  searchScope: SearchScope;
  filters: LibraryFilters;
  /** 全局一套排序；集合内顺序只会在集合视图里生效。 */
  sort: SortSpec[];
  /** 离开集合时暂存带“集合内顺序”的排序，回到集合再恢复。 */
  collectionSort: SortSpec[] | null;
}

export function defaultBrowseState(initialSort: SortSpec[] = DEFAULT_SORT_LEVELS): BrowseState {
  return {
    view: "all",
    collectionId: null,
    search: "",
    searchScope: "view",
    filters: emptyFilters(),
    sort: initialSort.length > 0 ? initialSort : DEFAULT_SORT_LEVELS,
    collectionSort: null,
  };
}

/** 当前视图实际使用的视图（全库搜索时临时按“全部”展示）。 */
export function effectiveViewOf(state: BrowseState): LibraryView {
  return state.search.trim() && state.searchScope === "library" ? "all" : state.view;
}

/** 查询使用的排序：非集合视图去掉“集合内顺序”，为空时回退默认排序。 */
export function effectiveSort(state: BrowseState): SortSpec[] {
  const view = effectiveViewOf(state);
  const sort =
    view === "collection"
      ? state.sort
      : state.sort.filter((spec) => spec.key !== "collectionPosition");
  return sort.length > 0 ? sort : DEFAULT_SORT_LEVELS;
}

/** 切换视图时如何安置排序：“集合内顺序”离开集合暂存，回到集合恢复。 */
export function sortForViewChange(
  state: BrowseState,
  view: LibraryView,
): { sort: SortSpec[]; collectionSort: SortSpec[] | null } {
  if (view === "collection") {
    return {
      sort: state.collectionSort ?? state.sort,
      collectionSort: state.collectionSort,
    };
  }
  if (state.sort.some((spec) => spec.key === "collectionPosition")) {
    const kept = state.sort.filter((spec) => spec.key !== "collectionPosition");
    return {
      sort: kept.length > 0 ? kept : DEFAULT_SORT_LEVELS,
      collectionSort: state.sort,
    };
  }
  return { sort: state.sort, collectionSort: state.collectionSort };
}

/** 跳到某个文件夹：离开回收站与临时视图，清空搜索和其它筛选；集合视图保持不变。 */
export function browseToFolder(state: BrowseState, folderId: number): BrowseState {
  const leaving = state.view === "temp" || state.view === "trash";
  return {
    ...state,
    ...(leaving ? sortForViewChange(state, "all") : {}),
    view: leaving ? "all" : state.view,
    collectionId: leaving ? null : state.collectionId,
    search: "",
    searchScope: "view",
    filters: { ...emptyFilters(), folderIds: [folderId], folderDirect: false },
  };
}

export function sortSummary(sort: SortSpec[]): string {
  if (sort.length === 0) return SORT_LABELS.importedAt;
  const [first, second] = sort;
  const head = `${SORT_LABELS[first.key]} ${first.descending ? "↓" : "↑"}`;
  if (!second) return head;
  const tail = `${SORT_LABELS[second.key]} ${second.descending ? "↓" : "↑"}`;
  return sort.length > 2 ? `${head} + ${tail} +${sort.length - 2}` : `${head} + ${tail}`;
}

/** 指标是否可用于当前视图；“集合内顺序”只能用于集合视图。 */
export function sortKeyAvailable(key: SortKey, view: LibraryView): boolean {
  return key !== "collectionPosition" || view === "collection";
}

export function emptyFilters(): LibraryFilters {
  return {
    folderIds: [],
    folderDirect: false,
    trashLoose: false,
    extensions: [],
    scores: [],
    tagIds: [],
    tagMode: "any",
    tagPresence: "any",
    excludeTagIds: [],
    importedFrom: null,
    importedTo: null,
    sizeMin: null,
    sizeMax: null,
    description: "any",
    embedding: "any",
    pose: "any",
  };
}

export function normalizeFilters(filters: LibraryFilters): LibraryFilters {
  const trashLoose = filters.trashLoose === true;
  return {
    folderIds: trashLoose
      ? []
      : [...new Set(filters.folderIds)].sort((a, b) => a - b),
    folderDirect:
      !trashLoose && filters.folderIds.length > 0 && filters.folderDirect === true,
    trashLoose,
    extensions: [...new Set(filters.extensions)].sort(),
    scores: [...new Set(filters.scores)].sort((a, b) => a - b),
    tagIds: filters.tagPresence === "missing"
      ? []
      : [...new Set(filters.tagIds)].sort((a, b) => a - b),
    tagMode: filters.tagMode,
    tagPresence: filters.tagPresence,
    excludeTagIds: [...new Set(filters.excludeTagIds ?? [])].sort((a, b) => a - b),
    importedFrom: filters.importedFrom,
    importedTo: filters.importedTo,
    sizeMin: filters.sizeMin,
    sizeMax: filters.sizeMax,
    description: filters.description,
    embedding: filters.embedding,
    pose: filters.pose,
  };
}

export function buildQuery(
  state: BrowseState,
  cursor: LibraryQuery["cursor"] = null,
): LibraryQuery {
  const search = state.search.trim();
  return {
    view: state.view,
    collectionId: state.view === "collection" ? state.collectionId : null,
    search,
    searchScope: search ? state.searchScope : "view",
    filters: normalizeFilters({
      ...state.filters,
      // “直属图片”只属于回收站视图；切到其它视图时自动失效。
      trashLoose: state.view === "trash" && state.filters.trashLoose,
    }),
    sort: effectiveSort(state),
    cursor,
    limit: PAGE_SIZE,
  };
}

export function queryKey(query: LibraryQuery): string {
  return JSON.stringify({
    view: query.view,
    collectionId: query.collectionId ?? null,
    search: query.search,
    searchScope: query.searchScope,
    filters: normalizeFilters(query.filters),
    sort: query.sort,
  });
}

/** 结果集身份：视图/集合/搜索/筛选。排序与数据刷新不改变身份，因此不进历史记录、不清空选择。 */
export function queryIdentityKey(query: LibraryQuery): string {
  return JSON.stringify({
    view: query.view,
    collectionId: query.collectionId ?? null,
    search: query.search,
    searchScope: query.searchScope,
    filters: normalizeFilters(query.filters),
  });
}

export function querySortKey(query: LibraryQuery): string {
  return JSON.stringify(query.sort);
}

export function activeFilterCount(filters: LibraryFilters): number {
  let count =
    filters.folderIds.length +
    filters.extensions.length +
    filters.scores.length +
    filters.tagIds.length;
  if (filters.importedFrom != null || filters.importedTo != null) count += 1;
  if (filters.sizeMin != null || filters.sizeMax != null) count += 1;
  if (filters.tagPresence !== "any") count += 1;
  if (filters.excludeTagIds.length > 0) count += 1;
  if (filters.description !== "any") count += 1;
  if (filters.embedding !== "any") count += 1;
  if (filters.pose !== "any") count += 1;
  return count;
}

export function hasActiveFilters(filters: LibraryFilters): boolean {
  return activeFilterCount(filters) > 0;
}

export function hasActiveQuery(state: BrowseState): boolean {
  return state.search.trim().length > 0 || hasActiveFilters(state.filters);
}

export type ImageAction = "trash" | "restore" | "delete";

/** 详情、批量与查看器共用：回收站视图只给恢复/永久删除，普通视图只给移入回收站。 */
export function imageActions(inTrash: boolean): ImageAction[] {
  return inTrash ? ["restore", "delete"] : ["trash"];
}

/** 单个标签在筛选面板里的三态：不参与、要求包含、要求排除。 */
export type TagOptionState = "neutral" | "include" | "exclude";

export function tagOptionState(filters: LibraryFilters, tagId: number): TagOptionState {
  if (filters.tagIds.includes(tagId)) return "include";
  if (filters.excludeTagIds.includes(tagId)) return "exclude";
  return "neutral";
}

/**
 * 点一次走一格：中性 → 包含 → 排除 → 中性。
 *
 * 包含与排除互斥：同一个标签不能既要求有又要求没有，切换时把另一边摘掉。
 * 从「无标签」状态下点标签视为包含，顺手把存在性条件让位。
 */
export function cycleTagFilter(filters: LibraryFilters, tagId: number): LibraryFilters {
  const state = tagOptionState(filters, tagId);
  if (state === "neutral") {
    return {
      ...filters,
      tagIds: [...filters.tagIds, tagId],
      excludeTagIds: filters.excludeTagIds.filter((id) => id !== tagId),
      tagPresence: "any",
    };
  }
  if (state === "include") {
    return {
      ...filters,
      tagIds: filters.tagIds.filter((id) => id !== tagId),
      excludeTagIds: [...filters.excludeTagIds, tagId],
    };
  }
  return {
    ...filters,
    excludeTagIds: filters.excludeTagIds.filter((id) => id !== tagId),
  };
}

export function toggleValue<T>(values: T[], value: T): T[] {
  return values.includes(value)
    ? values.filter((item) => item !== value)
    : [...values, value];
}

export function folderLabel(path: string): string {
  return path || "根目录";
}

// 体积格式化与查看器共用一份实现；`node --test` 直接跑 .ts 时需要显式扩展名。
export { formatBytes } from "../../shared/format.ts";

export function formatDimensions(width: number, height: number): string {
  return `${width.toLocaleString()} × ${height.toLocaleString()}`;
}

export function formatDate(timestamp: number): string {
  const date = new Date(timestamp);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}`;
}

export function formatScore(score: number): string {
  return score > 0 ? "★".repeat(score) : "—";
}

export function extensionLabel(extension: string): string {
  return extension.toUpperCase();
}

/** 以导入时按魔数识别的 format 为准，不用可能被改名的扩展名判断。 */
export function isGifImage(image: Pick<ImageRow, "format">): boolean {
  return image.format.toLowerCase() === "gif";
}

export interface TagFilterRequest {
  tagId: number;
  action: TagFilterAction;
  scope: TagFilterScope;
  /** 当前集合；scope = collection 且为空时该范围不可用。 */
  collectionId: number | null;
}

/**
 * 按标签筛选后的浏览状态。`view` 范围保留当前条件；文件夹/集合/全库范围清空搜索与
 * 其它筛选，只留下对应范围与这个标签。范围当前不可用时返回 `null`。
 */
export function tagFilterState(
  state: BrowseState,
  request: TagFilterRequest,
): BrowseState | null {
  const { tagId, action, scope, collectionId } = request;
  const selected = state.filters.tagIds;

  if (action === "remove") {
    if (!selected.includes(tagId)) return state;
    return {
      ...state,
      filters: {
        ...state.filters,
        tagIds: selected.filter((id) => id !== tagId),
        tagPresence: "any",
      },
    };
  }
  if (action === "append") {
    const tagIds = selected.includes(tagId)
      ? selected.filter((id) => id !== tagId)
      : [...selected, tagId];
    return {
      ...state,
      filters: { ...state.filters, tagIds, tagPresence: "any" },
    };
  }
  if (scope === "view") {
    return {
      ...state,
      filters: { ...state.filters, tagIds: [tagId], tagPresence: "any" },
    };
  }
  if (scope === "folder" && state.filters.folderIds.length === 0) return null;
  if (scope === "collection" && collectionId == null) return null;

  const view: LibraryView = scope === "collection" ? "collection" : "all";
  return {
    ...state,
    ...sortForViewChange(state, view),
    view,
    collectionId: view === "collection" ? collectionId : null,
    search: "",
    searchScope: "view",
    filters: {
      ...emptyFilters(),
      folderIds: scope === "folder" ? state.filters.folderIds : [],
      folderDirect: scope === "folder" ? state.filters.folderDirect : false,
      tagIds: [tagId],
    },
  };
}

/** 目标图是否落在标签筛选的新范围里；无法确认时按“不在”处理。 */
export function rowInTagScope(
  row: ImageRow,
  scope: TagFilterScope,
  context: {
    folderPaths: string[];
    folderDirect: boolean;
    hasCollection: boolean;
    similarityActive: boolean;
  },
): boolean {
  switch (scope) {
    case "folder":
      return context.folderPaths.some((path) =>
        context.folderDirect
          ? row.folderPath === path
          : path === "" || row.folderPath === path || row.folderPath.startsWith(`${path}/`),
      );
    case "collection":
      return context.hasCollection;
    case "library":
      return row.deletedAt == null;
    case "view":
      return !context.similarityActive;
  }
}

export function selectionRange(items: ImageRow[], anchorId: number, targetId: number): Set<number> {
  const anchorIndex = items.findIndex((item) => item.id === anchorId);
  const targetIndex = items.findIndex((item) => item.id === targetId);
  if (anchorIndex < 0 || targetIndex < 0) {
    return new Set([targetId]);
  }
  const start = Math.min(anchorIndex, targetIndex);
  const end = Math.max(anchorIndex, targetIndex);
  return new Set(items.slice(start, end + 1).map((item) => item.id));
}

export function isFormTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return ["INPUT", "TEXTAREA", "SELECT", "BUTTON", "A"].includes(target.tagName);
}

export function localDayStart(value: string): number | null {
  if (!value) return null;
  const [year, month, day] = value.split("-").map(Number);
  if (!year || !month || !day) return null;
  return new Date(year, month - 1, day, 0, 0, 0, 0).getTime();
}

export function localDayEnd(value: string): number | null {
  if (!value) return null;
  const [year, month, day] = value.split("-").map(Number);
  if (!year || !month || !day) return null;
  return new Date(year, month - 1, day, 23, 59, 59, 999).getTime();
}

export function dateInputValue(timestamp: number | null): string {
  if (timestamp == null) return "";
  const date = new Date(timestamp);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

export function mbToBytes(value: string): number | null {
  const trimmed = value.trim();
  if (!trimmed) return null;
  const number = Number(trimmed);
  if (!Number.isFinite(number) || number < 0) return null;
  return Math.round(number * 1024 * 1024);
}

export function bytesToMbInput(bytes: number | null): string {
  if (bytes == null) return "";
  return String(Math.round((bytes / (1024 * 1024)) * 10) / 10);
}

/** 语义搜索与目标图相似度的结果数据源；由 src/app 的控制器实现，LibraryView 只消费。 */
export interface SimilarityBrowseState {
  active: boolean;
  mode: "semantic" | "similar";
  text: string;
  kind: SimilarKind | null;
  targetIds: number[];
  scopeLabel: string;
  token: string | null;
  model: string;
  total: number;
  elapsedMs: number;
  busy: boolean;
  /** 语义搜索当前阶段（加载模型 / 计算均值 / 扫描向量）；再次搜索前会清空。 */
  stage: string | null;
  error: string | null;
  items: ImageRow[];
  status: "loading" | "ready" | "error";
  moreStatus: "idle" | "loading" | "error";
  moreError: string | null;
  hasMore: boolean;
  search: (text: string) => void;
  findSimilar: (config: FindSimilarConfig) => void;
  clear: () => void;
  loadMore: () => void;
  retry: () => void;
  ids: () => Promise<number[]>;
  patchItem: (imageId: number, update: Partial<ImageRow>) => void;
}
