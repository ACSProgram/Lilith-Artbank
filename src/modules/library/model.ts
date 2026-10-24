import type {
  ImageRow,
  LibraryFilters,
  LibraryQuery,
  LibraryView,
  SearchScope,
  SortKey,
  SortSpec,
} from "./types";
import type { SimilarKind } from "../../shared/similarity";

/** 与 Rust `query::DEFAULT_LIMIT` 保持一致。 */
export const PAGE_SIZE = 150;

export const DEFAULT_SORT: SortSpec = { key: "importedAt", descending: true };
export const COLLECTION_SORT: SortSpec = { key: "collectionPosition", descending: false };

export interface BrowseState {
  view: LibraryView;
  collectionId: number | null;
  search: string;
  searchScope: SearchScope;
  filters: LibraryFilters;
  sort: SortSpec;
}

export function defaultBrowseState(): BrowseState {
  return {
    view: "all",
    collectionId: null,
    search: "",
    searchScope: "view",
    filters: emptyFilters(),
    sort: DEFAULT_SORT,
  };
}

export function sortOptionsFor(view: LibraryView): { key: SortKey; label: string }[] {
  const base = [
    { key: "importedAt" as const, label: "导入时间" },
    { key: "score" as const, label: "评分" },
    { key: "size" as const, label: "文件大小" },
  ];
  return view === "collection"
    ? [{ key: "collectionPosition" as const, label: "集合内顺序" }, ...base]
    : base;
}

export function emptyFilters(): LibraryFilters {
  return {
    folderIds: [],
    extensions: [],
    scores: [],
    tagIds: [],
    tagMode: "any",
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
  return {
    folderIds: [...new Set(filters.folderIds)].sort((a, b) => a - b),
    extensions: [...new Set(filters.extensions)].sort(),
    scores: [...new Set(filters.scores)].sort((a, b) => a - b),
    tagIds: [...new Set(filters.tagIds)].sort((a, b) => a - b),
    tagMode: filters.tagMode,
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
    filters: normalizeFilters(state.filters),
    sort: state.sort,
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

/** 只反映筛选与视图；排序变化不应清空图片选择。 */
export function filterKey(query: LibraryQuery): string {
  return JSON.stringify({
    view: query.view,
    collectionId: query.collectionId ?? null,
    search: query.search,
    searchScope: query.searchScope,
    filters: normalizeFilters(query.filters),
  });
}

export function activeFilterCount(filters: LibraryFilters): number {
  let count =
    filters.folderIds.length +
    filters.extensions.length +
    filters.scores.length +
    filters.tagIds.length;
  if (filters.importedFrom != null || filters.importedTo != null) count += 1;
  if (filters.sizeMin != null || filters.sizeMax != null) count += 1;
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

export function toggleValue<T>(values: T[], value: T): T[] {
  return values.includes(value)
    ? values.filter((item) => item !== value)
    : [...values, value];
}

export function folderLabel(path: string): string {
  return path || "根目录";
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

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
  targetId: number | null;
  token: string | null;
  model: string;
  total: number;
  elapsedMs: number;
  busy: boolean;
  error: string | null;
  items: ImageRow[];
  status: "loading" | "ready" | "error";
  moreStatus: "idle" | "loading" | "error";
  moreError: string | null;
  hasMore: boolean;
  search: (text: string) => void;
  findSimilar: (
    kind: SimilarKind,
    targetId: number,
    threshold?: number,
  ) => void;
  clear: () => void;
  loadMore: () => void;
  retry: () => void;
  ids: () => Promise<number[]>;
}
