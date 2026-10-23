import { MOCK_IMAGE_COUNT } from "./mockConfig";
import type {
  ColorEntry,
  CollectionInfo,
  CombineMode,
  DeleteReport,
  FilterCounts,
  FolderCount,
  FolderInfo,
  ImageDetails,
  ImagePage,
  ImageRow,
  LibraryFilters,
  LibraryQuery,
  MoveReport,
  QueryCursor,
  TagCount,
  TagInfo,
  TagRef,
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

const folderPaths = [...FOLDER_PATHS];
const tagState = TAG_NAMES.map((name, index) => ({ id: index + 1, name }));
const descriptions = new Map<number, string>();
const palettes = new Map<number, { colors4: ColorEntry[]; colors8: ColorEntry[] }>();
let nextFolderId = folderPaths.length + 1;
let nextTagId = tagState.length + 1;
let nextCollectionId = 1;
let collections: CollectionInfo[] = [];
const collectionItems = new Map<number, number[]>();

function hashOf(index: number): string {
  return (index + 1).toString(16).padStart(64, "0");
}

function formatOf(extension: string): string {
  return extension === "jpg" ? "jpeg" : extension;
}

function filePathOf(contentHash: string, extension: string, folderPath: string): string {
  const folderPart = folderPath ? `${folderPath.replaceAll("/", "\\")}\\` : "";
  return `G:\\Mock Library\\${folderPart}${contentHash}.${extension}`;
}

function createMockRows(count: number): ImageRow[] {
  const rows: ImageRow[] = [];
  for (let index = 0; index < count; index += 1) {
    const id = index + 1;
    const folderIndex = index % folderPaths.length;
    const folderPath = folderPaths[folderIndex];
    const extension = EXTENSIONS[index % EXTENSIONS.length];
    const [width, height] = DIMENSIONS[index % DIMENSIONS.length];
    const contentHash = hashOf(index);
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
      filePath: filePathOf(contentHash, extension, folderPath),
      fileSize: 180_000 + (index % 37) * 120_000,
      width,
      height,
      format: formatOf(extension),
      hasAlpha: extension === "png" || extension === "webp",
      score: index % 6,
      importedAt: 1_760_000_000_000 + index * 60_000,
      modifiedAt: 1_760_000_000_000 + index * 60_000,
      deletedAt: null,
      thumbVersion: 1,
      thumbWidth: Math.min(width, 512),
      thumbHeight: Math.round((Math.min(width, 512) / width) * height),
      tags: [...tagIds].map((tagId) => TAG_NAMES[tagId - 1]),
    });
    if (index % 7 === 0) descriptions.set(id, `人物速写 第${id}张`);
    else if (index % 11 === 0) descriptions.set(id, `风景参考 ${id}`);
  }
  return rows;
}

const mockImages = createMockRows(MOCK_IMAGE_COUNT);

export function mockFolders(): FolderInfo[] {
  return folderPaths.map((path, index) => ({
    id: index + 1,
    path,
    imageCount: mockImages.filter((image) => image.folderId === index + 1 && image.deletedAt == null)
      .length,
  }));
}

export function mockTags(): TagInfo[] {
  return tagState
    .map((tag) => ({
      id: tag.id,
      name: tag.name,
      imageCount: mockImages.filter(
        (image) => image.deletedAt == null && image.tags.includes(tag.name),
      ).length,
    }))
    .sort((a, b) => b.imageCount - a.imageCount || a.name.localeCompare(b.name));
}

export function mockSearchTags(query: string, limit = 50): TagInfo[] {
  const needle = query.trim().toLowerCase();
  return mockTags()
    .filter((tag) => (needle ? tag.name.toLowerCase().includes(needle) : true))
    .slice(0, limit);
}

export function mockLibraryCounts(): { imageCount: number; trashCount: number } {
  return {
    imageCount: mockImages.filter((image) => image.deletedAt == null).length,
    trashCount: mockImages.filter((image) => image.deletedAt != null).length,
  };
}

type Facet = "folders" | "extensions" | "scores" | "tags" | null;

function isLibraryScope(query: LibraryQuery): boolean {
  return query.search.trim().length > 0 && query.searchScope === "library";
}

function inView(image: ImageRow, query: LibraryQuery): boolean {
  if (isLibraryScope(query)) return true;
  if (query.view === "trash") return image.deletedAt != null;
  if (query.view === "collection") {
    if (image.deletedAt != null) return false;
    const collectionId = query.collectionId ?? -1;
    return (collectionItems.get(collectionId) ?? []).includes(image.id);
  }
  return image.deletedAt == null;
}

function matchesSearch(image: ImageRow, query: LibraryQuery): boolean {
  const needle = query.search.trim().toLowerCase();
  if (!needle) return true;
  if ((descriptions.get(image.id) ?? "").toLowerCase().includes(needle)) return true;
  if (image.tags.some((tag) => tag.toLowerCase().includes(needle))) return true;
  return image.folderPath.toLowerCase().includes(needle);
}

function tagNameOf(tagId: number): string | undefined {
  return tagState.find((tag) => tag.id === tagId)?.name;
}

function matchesFilters(image: ImageRow, filters: LibraryFilters, exclude: Facet): boolean {
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
    const names = filters.tagIds.map(tagNameOf).filter((name): name is string => Boolean(name));
    const matched = names.filter((name) => image.tags.includes(name)).length;
    if (filters.tagMode === "all" ? matched !== names.length : matched === 0) return false;
  }
  if (filters.importedFrom != null && image.importedAt < filters.importedFrom) return false;
  if (filters.importedTo != null && image.importedAt > filters.importedTo) return false;
  if (filters.sizeMin != null && image.fileSize < filters.sizeMin) return false;
  if (filters.sizeMax != null && image.fileSize > filters.sizeMax) return false;

  const hasDescription = (descriptions.get(image.id) ?? "").length > 0;
  if (filters.description === "has" && !hasDescription) return false;
  if (filters.description === "missing" && hasDescription) return false;
  if (filters.embedding === "has" || filters.pose === "has") return false;
  return true;
}

function matches(image: ImageRow, query: LibraryQuery, exclude: Facet): boolean {
  return inView(image, query) && matchesSearch(image, query) && matchesFilters(image, query.filters, exclude);
}

function sortValue(image: ImageRow, query: LibraryQuery): number {
  if (query.sort.key === "collectionPosition") {
    const collectionId = query.collectionId ?? -1;
    return (collectionItems.get(collectionId) ?? []).indexOf(image.id);
  }
  if (query.sort.key === "score") return image.score;
  if (query.sort.key === "size") return image.fileSize;
  return image.importedAt;
}

function compareImages(a: ImageRow, b: ImageRow, query: LibraryQuery): number {
  const direction = query.sort.descending ? -1 : 1;
  const difference = sortValue(a, query) - sortValue(b, query);
  if (difference !== 0) return difference * direction;
  return (a.id - b.id) * direction;
}

function afterCursor(image: ImageRow, query: LibraryQuery): boolean {
  const cursor = query.cursor;
  if (!cursor) return true;
  const key = Number(cursor.sortKey);
  const difference = sortValue(image, query) - key;
  if (difference !== 0) {
    return query.sort.descending ? difference < 0 : difference > 0;
  }
  return query.sort.descending ? image.id < cursor.id : image.id > cursor.id;
}

export function mockQueryPage(query: LibraryQuery): ImagePage {
  const limit = Math.min(query.limit ?? 150, 300);
  const items = mockImages
    .filter((image) => matches(image, query, null) && afterCursor(image, query))
    .sort((a, b) => compareImages(a, b, query))
    .slice(0, limit);
  const last = items[items.length - 1];
  const nextCursor: QueryCursor | null =
    items.length > 0 && items.length === limit
      ? { sortKey: String(sortValue(last, query)), id: last.id }
      : null;
  return { items, nextCursor };
}

export function mockQueryIds(query: LibraryQuery): number[] {
  return mockImages
    .filter((image) => matches(image, query, null))
    .sort((a, b) => compareImages(a, b, query))
    .map((image) => image.id);
}

function groupCounts(rows: ImageRow[], key: (image: ImageRow) => string): Map<string, number> {
  const counts = new Map<string, number>();
  for (const row of rows) {
    const value = key(row);
    counts.set(value, (counts.get(value) ?? 0) + 1);
  }
  return counts;
}

export function mockFilterCounts(query: LibraryQuery): FilterCounts {
  const total = mockImages.filter((image) => matches(image, query, null)).length;
  const folderCounts = groupCounts(
    mockImages.filter((image) => matches(image, query, "folders")),
    (image) => String(image.folderId),
  );
  const extensionCounts = groupCounts(
    mockImages.filter((image) => matches(image, query, "extensions")),
    (image) => image.fileExt,
  );
  const scoreCounts = groupCounts(
    mockImages.filter((image) => matches(image, query, "scores")),
    (image) => String(image.score),
  );
  const tagCounts = new Map<string, number>();
  for (const image of mockImages.filter((item) => matches(item, query, "tags"))) {
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
    .map(([name, count]) => ({ tagId: tagState.find((tag) => tag.name === name)?.id ?? 0, count }))
    .filter((item) => item.tagId > 0)
    .sort((a, b) => b.count - a.count || a.tagId - b.tagId);

  return { total, folders, extensions, scores, tags };
}

function paletteFor(id: number): { colors4: ColorEntry[]; colors8: ColorEntry[] } {
  const cached = palettes.get(id);
  if (cached) return cached;
  const build = (count: number): ColorEntry[] => {
    const entries = Array.from({ length: count }, (_, index) => {
      const seed = id * 2654435761 + index * 40503;
      return {
        r: (seed >> 8) & 0xff,
        g: (seed >> 16) & 0xff,
        b: (seed >> 24) & 0xff,
        weight: 0,
      };
    });
    const base = Math.floor(255 / count);
    let remaining = 255 - base * count;
    for (const entry of entries) {
      entry.weight = base + (remaining > 0 ? 1 : 0);
      if (remaining > 0) remaining -= 1;
    }
    return entries;
  };
  const value = { colors4: build(4), colors8: build(8) };
  palettes.set(id, value);
  return value;
}

function tagRefs(image: ImageRow): TagRef[] {
  return image.tags.map((name) => ({
    id: tagState.find((tag) => tag.name === name)?.id ?? 0,
    name,
  }));
}

export function mockImageDetail(id: number): ImageDetails {
  const image = mockImages.find((item) => item.id === id);
  if (!image) throw new Error("图片不存在");
  const palette = paletteFor(id);
  return {
    ...image,
    description: descriptions.get(id) ?? "",
    descriptionSource: "user",
    descriptionModel: "",
    tags: tagRefs(image),
    colors4: palette.colors4,
    colors8: palette.colors8,
  };
}

function findImage(id: number): ImageRow {
  const image = mockImages.find((item) => item.id === id);
  if (!image) throw new Error("图片不存在");
  return image;
}

export function mockUpdateScore(id: number, score: number): void {
  findImage(id).score = score;
}

export function mockSetScores(ids: number[], score: number): number {
  let changed = 0;
  for (const id of ids) {
    const image = mockImages.find((item) => item.id === id);
    if (image) {
      image.score = score;
      changed += 1;
    }
  }
  return changed;
}

export function mockUpdateDescription(id: number, description: string): void {
  const text = description.trim();
  if (text) descriptions.set(id, text);
  else descriptions.delete(id);
  findImage(id).modifiedAt = Date.now();
}

function ensureTag(name: string): number {
  const trimmed = name.trim();
  const existing = tagState.find((tag) => tag.name.toLowerCase() === trimmed.toLowerCase());
  if (existing) return existing.id;
  const tag = { id: nextTagId++, name: trimmed };
  tagState.push(tag);
  return tag.id;
}

function normalizeTagNames(names: string[]): string[] {
  const result: string[] = [];
  const seen = new Set<string>();
  for (const name of names) {
    const trimmed = name.trim();
    if (!trimmed || seen.has(trimmed.toLowerCase())) continue;
    seen.add(trimmed.toLowerCase());
    result.push(trimmed);
  }
  return result.slice(0, 64);
}

export function mockSetImageTags(id: number, names: string[]): TagRef[] {
  const image = findImage(id);
  const normalized = normalizeTagNames(names);
  normalized.forEach(ensureTag);
  image.tags = normalized;
  image.modifiedAt = Date.now();
  return tagRefs(image);
}

export function mockAddTags(ids: number[], names: string[]): number {
  const normalized = normalizeTagNames(names);
  normalized.forEach(ensureTag);
  let added = 0;
  for (const id of ids) {
    const image = mockImages.find((item) => item.id === id);
    if (!image) continue;
    for (const name of normalized) {
      if (!image.tags.includes(name)) {
        image.tags.push(name);
        added += 1;
      }
    }
  }
  return added;
}

export function mockRemoveTags(ids: number[], names: string[]): number {
  const normalized = normalizeTagNames(names);
  let removed = 0;
  for (const id of ids) {
    const image = mockImages.find((item) => item.id === id);
    if (!image) continue;
    const before = image.tags.length;
    image.tags = image.tags.filter((tag) => !normalized.includes(tag));
    removed += before - image.tags.length;
  }
  return removed;
}

export function mockRenameTag(tagId: number, name: string): void {
  const tag = tagState.find((item) => item.id === tagId);
  if (!tag) throw new Error("标签不存在");
  const trimmed = name.trim();
  if (!trimmed) throw new Error("标签名不能为空");
  if (tagState.some((item) => item.id !== tagId && item.name.toLowerCase() === trimmed.toLowerCase())) {
    throw new Error("已经有同名标签");
  }
  const oldName = tag.name;
  tag.name = trimmed;
  for (const image of mockImages) {
    image.tags = image.tags.map((value) => (value === oldName ? trimmed : value));
  }
}

export function mockDeleteTag(tagId: number): number {
  const tag = tagState.find((item) => item.id === tagId);
  if (!tag) throw new Error("标签不存在");
  const index = tagState.indexOf(tag);
  tagState.splice(index, 1);
  let affected = 0;
  for (const image of mockImages) {
    const before = image.tags.length;
    image.tags = image.tags.filter((name) => name !== tag.name);
    affected += before - image.tags.length;
  }
  return affected;
}

export function mockMoveImages(ids: number[], folderPath: string): MoveReport {
  const normalized = folderPath.replaceAll("\\", "/").trim().replace(/^\/+|\/+$/g, "");
  let folderId = folderPaths.indexOf(normalized) + 1;
  if (folderId === 0) {
    folderPaths.push(normalized);
    folderId = nextFolderId;
    nextFolderId += 1;
  }
  let moved = 0;
  let skipped = 0;
  for (const id of ids) {
    const image = mockImages.find((item) => item.id === id);
    if (!image) continue;
    if (image.folderPath === normalized) {
      skipped += 1;
      continue;
    }
    image.folderPath = normalized;
    image.folderId = folderId;
    image.filePath = filePathOf(image.contentHash, image.fileExt, normalized);
    image.modifiedAt = Date.now();
    moved += 1;
  }
  return { moved, skipped, failures: [] };
}

export function mockTrashImages(ids: number[]): number {
  const now = Date.now();
  let changed = 0;
  for (const id of ids) {
    const image = mockImages.find((item) => item.id === id);
    if (image && image.deletedAt == null) {
      image.deletedAt = now;
      changed += 1;
    }
  }
  return changed;
}

export function mockRestoreImages(ids: number[]): number {
  let changed = 0;
  for (const id of ids) {
    const image = mockImages.find((item) => item.id === id);
    if (image && image.deletedAt != null) {
      image.deletedAt = null;
      changed += 1;
    }
  }
  return changed;
}

export function mockDeleteImages(ids: number[], onlyTrashed = false): DeleteReport {
  const targets = mockImages.filter(
    (image) => ids.includes(image.id) && (!onlyTrashed || image.deletedAt != null),
  );
  const targetIds = new Set(targets.map((image) => image.id));
  for (let index = mockImages.length - 1; index >= 0; index -= 1) {
    if (targetIds.has(mockImages[index].id)) mockImages.splice(index, 1);
  }
  for (const [collectionId, items] of collectionItems) {
    collectionItems.set(
      collectionId,
      items.filter((id) => !targetIds.has(id)),
    );
  }
  for (const id of targetIds) {
    descriptions.delete(id);
    palettes.delete(id);
  }
  return { deleted: targetIds.size, filesRemoved: targetIds.size, failures: [] };
}

export function mockEmptyTrash(): DeleteReport {
  return mockDeleteImages(
    mockImages.filter((image) => image.deletedAt != null).map((image) => image.id),
    true,
  );
}

export function mockCollections(): CollectionInfo[] {
  return collections
    .map(collectionInfo)
    .sort((a, b) => a.sortOrder - b.sortOrder || a.id - b.id);
}

function collectionInfo(collection: CollectionInfo): CollectionInfo {
  const items = collectionItems.get(collection.id) ?? [];
  const imageCount = items.filter((id) => {
    const image = mockImages.find((item) => item.id === id);
    return image && image.deletedAt == null;
  }).length;
  return { ...collection, imageCount };
}

export function mockCreateCollection(name: string): CollectionInfo {
  const trimmed = name.trim();
  if (!trimmed) throw new Error("集合名称不能为空");
  if (collections.some((item) => item.name.toLowerCase() === trimmed.toLowerCase())) {
    throw new Error("已经有同名集合");
  }
  const now = Date.now();
  const info: CollectionInfo = {
    id: nextCollectionId++,
    name: trimmed,
    sortOrder: collections.length,
    imageCount: 0,
    createdAt: now,
    updatedAt: now,
  };
  collections = [...collections, info];
  collectionItems.set(info.id, []);
  return { ...info };
}

export function mockRenameCollection(id: number, name: string): void {
  const trimmed = name.trim();
  if (!trimmed) throw new Error("集合名称不能为空");
  if (collections.some((item) => item.id !== id && item.name.toLowerCase() === trimmed.toLowerCase())) {
    throw new Error("已经有同名集合");
  }
  const collection = collections.find((item) => item.id === id);
  if (!collection) throw new Error("集合不存在");
  collection.name = trimmed;
  collection.updatedAt = Date.now();
}

export function mockDeleteCollection(id: number): number {
  const collection = collections.find((item) => item.id === id);
  if (!collection) throw new Error("集合不存在");
  const count = (collectionItems.get(id) ?? []).length;
  collections = collections.filter((item) => item.id !== id);
  collectionItems.delete(id);
  return count;
}

export function mockReorderCollections(ids: number[]): void {
  if (collections.length !== ids.length || !ids.every((id) => collections.some((item) => item.id === id))) {
    throw new Error("排序必须包含全部集合");
  }
  ids.forEach((id, index) => {
    const collection = collections.find((item) => item.id === id);
    if (collection) {
      collection.sortOrder = index;
      collection.updatedAt = Date.now();
    }
  });
}

export function mockAddToCollection(collectionId: number, imageIds: number[]): number {
  if (!collections.some((item) => item.id === collectionId)) throw new Error("集合不存在");
  const items = collectionItems.get(collectionId) ?? [];
  const existing = new Set(items);
  let added = 0;
  for (const id of imageIds) {
    if (!existing.has(id) && mockImages.some((image) => image.id === id)) {
      items.push(id);
      existing.add(id);
      added += 1;
    }
  }
  collectionItems.set(collectionId, items);
  const collection = collections.find((item) => item.id === collectionId);
  if (collection) collection.updatedAt = Date.now();
  return added;
}

export function mockRemoveFromCollection(collectionId: number, imageIds: number[]): number {
  const items = collectionItems.get(collectionId) ?? [];
  const remove = new Set(imageIds);
  const next = items.filter((id) => !remove.has(id));
  collectionItems.set(collectionId, next);
  return items.length - next.length;
}

export function mockCombineCollections(
  mode: CombineMode,
  collectionIds: number[],
  name: string,
): CollectionInfo {
  if (collectionIds.length < 2) throw new Error("集合运算至少需要两个集合");
  const lists = collectionIds.map((id) => collectionItems.get(id) ?? []);
  const first = lists[0] ?? [];
  let combined: number[] = [];
  if (mode === "union") {
    const seen = new Set<number>();
    for (const list of lists) {
      for (const id of list) {
        if (!seen.has(id)) {
          seen.add(id);
          combined.push(id);
        }
      }
    }
  } else if (mode === "intersection") {
    combined = first.filter((id) => lists.slice(1).every((list) => list.includes(id)));
  } else {
    const others = new Set(lists.slice(1).flat());
    combined = first.filter((id) => !others.has(id));
  }
  combined = [...new Set(combined)];
  const info = mockCreateCollection(name);
  mockAddToCollection(info.id, combined);
  return collectionInfo(info);
}
