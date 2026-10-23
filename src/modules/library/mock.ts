import { MOCK_IMAGE_COUNT } from "./mockConfig";
import type {
  FilterCounts,
  FolderCount,
  FolderInfo,
  ImagePage,
  ImageRow,
  LibraryQuery,
  QueryCursor,
  SortKey,
  TagCount,
  TagInfo,
  ValueCount,
} from "./types";

const FOLDER_PATHS = ["", "人物", "人物/速写", "风景", "概念设计", "参考/材质"];
const TAG_NAMES = ["速写", "参考", "练习", "色彩", "人设", "场景", "材质", "光影"];
const EXTENSIONS = ["jpg", "png", "webp", "tif", "bmp", "gif"];
const DIMENSIONS: [number, number][] = [
  [1920, 1080],
  [1600, 1200],
  [1080, 1350],
  [2048, 2048],
  [3840, 2160],
  [900, 1600],
];

function hashOf(index: number): string {
  return (index + 1).toString(16).padStart(64, "0");
}

function formatOf(extension: string): string {
  return extension === "jpg" ? "jpeg" : extension;
}

function createMockRows(count: number): ImageRow[] {
  const rows: ImageRow[] = [];
  for (let index = 0; index < count; index += 1) {
    const id = index + 1;
    const folderIndex = index % FOLDER_PATHS.length;
    const folderPath = FOLDER_PATHS[folderIndex];
    const extension = EXTENSIONS[index % EXTENSIONS.length];
    const [width, height] = DIMENSIONS[index % DIMENSIONS.length];
    const contentHash = hashOf(index);
    const folderPart = folderPath ? `${folderPath.replaceAll("/", "\\")}\\` : "";
    const tagIds = new Set([
      (index % TAG_NAMES.length) + 1,
      ((index * 3 + 2) % TAG_NAMES.length) + 1,
    ]);
    rows.push({
      id,
      contentHash,
      fileExt: extension,
      folderId: folderIndex + 1,
      folderPath,
      filePath: `G:\\Mock Library\\${folderPart}${contentHash}.${extension}`,
      fileSize: 180_000 + (index % 37) * 120_000,
      width,
      height,
      format: formatOf(extension),
      hasAlpha: extension === "png" || extension === "webp",
      score: index % 6,
      importedAt: 1_760_000_000_000 + index * 60_000,
      modifiedAt: 1_760_000_000_000 + index * 60_000,
      thumbVersion: 1,
      thumbWidth: Math.min(width, 512),
      thumbHeight: Math.round((Math.min(width, 512) / width) * height),
      tags: [...tagIds].map((tagId) => TAG_NAMES[tagId - 1]),
    });
  }
  return rows;
}

const mockImages = createMockRows(MOCK_IMAGE_COUNT);

export const mockFolders: FolderInfo[] = FOLDER_PATHS.map((path, index) => ({
  id: index + 1,
  path,
  imageCount: mockImages.filter((image) => image.folderId === index + 1).length,
}));

export const mockTags: TagInfo[] = TAG_NAMES.map((name, index) => ({
  id: index + 1,
  name,
  imageCount: mockImages.filter((image) => image.tags.includes(name)).length,
}));

type Facet = "folders" | "extensions" | "scores" | "tags" | null;

function matchesFilters(image: ImageRow, query: LibraryQuery, exclude: Facet): boolean {
  const { filters } = query;
  if (exclude !== "folders" && filters.folderIds.length > 0) {
    if (!filters.folderIds.includes(image.folderId)) return false;
  }
  if (exclude !== "extensions" && filters.extensions.length > 0) {
    if (!filters.extensions.includes(image.fileExt)) return false;
  }
  if (exclude !== "scores" && filters.scores.length > 0) {
    if (!filters.scores.includes(image.score)) return false;
  }
  if (exclude !== "tags" && filters.tagIds.length > 0) {
    const names = filters.tagIds.map((tagId) => TAG_NAMES[tagId - 1]).filter(Boolean);
    const matched = names.filter((name) => image.tags.includes(name)).length;
    if (filters.tagMode === "all" ? matched !== names.length : matched === 0) return false;
  }
  return true;
}

function sortValue(image: ImageRow, key: SortKey): number {
  if (key === "score") return image.score;
  if (key === "size") return image.fileSize;
  return image.importedAt;
}

function compareImages(a: ImageRow, b: ImageRow, query: LibraryQuery): number {
  const direction = query.sort.descending ? -1 : 1;
  const difference = sortValue(a, query.sort.key) - sortValue(b, query.sort.key);
  if (difference !== 0) return difference * direction;
  return (a.id - b.id) * direction;
}

function afterCursor(
  image: ImageRow,
  cursor: QueryCursor | null | undefined,
  query: LibraryQuery,
): boolean {
  if (!cursor) return true;
  const key = Number(cursor.sortKey);
  const difference = sortValue(image, query.sort.key) - key;
  if (difference !== 0) {
    return query.sort.descending ? difference < 0 : difference > 0;
  }
  return query.sort.descending ? image.id < cursor.id : image.id > cursor.id;
}

export function mockQueryPage(query: LibraryQuery): ImagePage {
  const limit = Math.min(query.limit ?? 150, 300);
  const items = mockImages
    .filter((image) => matchesFilters(image, query, null))
    .filter((image) => afterCursor(image, query.cursor, query))
    .sort((a, b) => compareImages(a, b, query))
    .slice(0, limit);
  const last = items[items.length - 1];
  const nextCursor: QueryCursor | null =
    items.length > 0 && items.length === limit
      ? { sortKey: String(sortValue(last, query.sort.key)), id: last.id }
      : null;
  return { items, nextCursor };
}

export function mockQueryIds(query: LibraryQuery): number[] {
  return mockImages
    .filter((image) => matchesFilters(image, query, null))
    .sort((a, b) => compareImages(a, b, query))
    .map((image) => image.id);
}

function groupCounts(
  rows: ImageRow[],
  key: (image: ImageRow) => string,
): Map<string, number> {
  const counts = new Map<string, number>();
  for (const row of rows) {
    const value = key(row);
    counts.set(value, (counts.get(value) ?? 0) + 1);
  }
  return counts;
}

export function mockFilterCounts(query: LibraryQuery): FilterCounts {
  const total = mockImages.filter((image) => matchesFilters(image, query, null)).length;
  const folderCounts = groupCounts(
    mockImages.filter((image) => matchesFilters(image, query, "folders")),
    (image) => String(image.folderId),
  );
  const extensionCounts = groupCounts(
    mockImages.filter((image) => matchesFilters(image, query, "extensions")),
    (image) => image.fileExt,
  );
  const scoreCounts = groupCounts(
    mockImages.filter((image) => matchesFilters(image, query, "scores")),
    (image) => String(image.score),
  );
  const tagRows = mockImages.filter((image) => matchesFilters(image, query, "tags"));
  const tagCounts = new Map<string, number>();
  for (const image of tagRows) {
    for (const tag of image.tags) {
      tagCounts.set(tag, (tagCounts.get(tag) ?? 0) + 1);
    }
  }

  const folders: FolderCount[] = [...folderCounts.entries()]
    .map(([folderId, count]) => ({ folderId: Number(folderId), count }))
    .sort((a, b) => b.count - a.count || a.folderId - b.folderId);
  const extensions: ValueCount[] = [...extensionCounts.entries()]
    .map(([value, count]) => ({ value, count }))
    .sort((a, b) => b.count - a.count || a.value.localeCompare(b.value));
  const scores: ValueCount[] = [...scoreCounts.entries()]
    .map(([value, count]) => ({ value, count }))
    .sort((a, b) => Number(b.value) - Number(a.value));
  const tags: TagCount[] = [...tagCounts.entries()]
    .map(([name, count]) => ({ tagId: TAG_NAMES.indexOf(name) + 1, count }))
    .sort((a, b) => b.count - a.count || a.tagId - b.tagId);

  return { total, folders, extensions, scores, tags };
}
