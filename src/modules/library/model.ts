import type { ImageRow, LibraryFilters, LibraryQuery, SortKey, SortSpec } from "./types";

/** 与 Rust `query::DEFAULT_LIMIT` 保持一致。 */
export const PAGE_SIZE = 150;

export const DEFAULT_SORT: SortSpec = { key: "importedAt", descending: true };

export const sortOptions: { key: SortKey; label: string }[] = [
  { key: "importedAt", label: "导入时间" },
  { key: "score", label: "评分" },
  { key: "size", label: "文件大小" },
];

export function emptyFilters(): LibraryFilters {
  return {
    folderIds: [],
    extensions: [],
    scores: [],
    tagIds: [],
    tagMode: "any",
  };
}

export function normalizeFilters(filters: LibraryFilters): LibraryFilters {
  return {
    folderIds: [...new Set(filters.folderIds)].sort((a, b) => a - b),
    extensions: [...new Set(filters.extensions)].sort(),
    scores: [...new Set(filters.scores)].sort((a, b) => a - b),
    tagIds: [...new Set(filters.tagIds)].sort((a, b) => a - b),
    tagMode: filters.tagMode,
  };
}

export function buildQuery(
  filters: LibraryFilters,
  sort: SortSpec,
  cursor: LibraryQuery["cursor"] = null,
): LibraryQuery {
  return {
    view: "all",
    search: "",
    filters: normalizeFilters(filters),
    sort,
    cursor,
    limit: PAGE_SIZE,
  };
}

export function queryKey(query: LibraryQuery): string {
  return JSON.stringify({
    view: query.view,
    search: query.search,
    filters: normalizeFilters(query.filters),
    sort: query.sort,
  });
}

/** 只反映筛选条件；排序变化不应清空图片选择。 */
export function filterKey(query: LibraryQuery): string {
  return JSON.stringify({
    view: query.view,
    search: query.search,
    filters: normalizeFilters(query.filters),
  });
}

export function activeFilterCount(filters: LibraryFilters): number {
  return (
    filters.folderIds.length +
    filters.extensions.length +
    filters.scores.length +
    filters.tagIds.length
  );
}

export function hasActiveFilters(filters: LibraryFilters): boolean {
  return activeFilterCount(filters) > 0;
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
