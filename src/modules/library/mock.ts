import { MOCK_IMAGE_COUNT } from "./mockConfig";
import { isSameOrDescendant } from "./folderTreeModel";
import type { TaskKind, TaskSnapshot, TaskStartResult } from "../../app/taskTypes";
import type {
  ColorEntry,
  CollectionInfo,
  CombineMode,
  DeleteReport,
  FilterCounts,
  FolderCount,
  FolderInfo,
  HistoryEntry,
  HistoryPage,
  ImageDetails,
  ImagePage,
  ImageRow,
  LibraryFilters,
  LibraryQuery,
  MoveReport,
  QueryCursor,
  SortKey,
  TagCount,
  TagInfo,
  TagRef,
  ValueCount,
} from "./types";

const FOLDER_PATHS = [
  "",
  "人物",
  "人物/速写",
  "风景",
  "概念设计",
  "参考",
  "参考/材质",
  "人物/速写/手部",
  "灵感",
  "灵感/待整理",
];
const IMAGE_FOLDER_PATHS = ["", "人物", "人物/速写", "风景", "概念设计", "参考/材质"];
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

function parentOf(path: string): string {
  const index = path.lastIndexOf("/");
  return index < 0 ? "" : path.slice(0, index);
}

const folderState = FOLDER_PATHS.map((path, index) => ({
  id: index + 1,
  path,
  sortIndex: 0,
}));
{
  const counters = new Map<string, number>();
  for (const folder of folderState) {
    const parent = parentOf(folder.path);
    const next = counters.get(parent) ?? 0;
    folder.sortIndex = next;
    counters.set(parent, next + 1);
  }
}
const tagState = TAG_NAMES.map((name, index) => ({ id: index + 1, name }));
const descriptions = new Map<number, string>();
const palettes = new Map<number, { colors4: ColorEntry[]; colors8: ColorEntry[] }>();
let nextFolderId = folderState.length + 1;
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
    const folderIndex = index % IMAGE_FOLDER_PATHS.length;
    const folderPath = IMAGE_FOLDER_PATHS[folderIndex];
    const folderId = folderState.find((folder) => folder.path === folderPath)?.id ?? 1;
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
      folderId,
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
  const direct = new Map<number, number>();
  for (const image of mockImages) {
    if (image.deletedAt != null) continue;
    direct.set(image.folderId, (direct.get(image.folderId) ?? 0) + 1);
  }
  const byPath = new Map(folderState.map((folder) => [folder.path, folder.id]));
  const totals = new Map<number, number>(folderState.map((folder) => [folder.id, 0]));
  for (const folder of folderState) {
    const count = direct.get(folder.id) ?? 0;
    if (count === 0) continue;
    let current: string | null = folder.path;
    while (current !== null) {
      const id = byPath.get(current);
      if (id != null) totals.set(id, (totals.get(id) ?? 0) + count);
      if (current === "") break;
      const index = current.lastIndexOf("/");
      current = index < 0 ? "" : current.slice(0, index);
    }
  }
  return folderState
    .map((folder) => ({
      id: folder.id,
      path: folder.path,
      sortIndex: folder.sortIndex,
      directImageCount: direct.get(folder.id) ?? 0,
      totalImageCount: totals.get(folder.id) ?? 0,
    }))
    .sort((left, right) => left.path.localeCompare(right.path, "zh-Hans-CN"));
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

function folderPathOf(folderId: number): string | undefined {
  return folderState.find((folder) => folder.id === folderId)?.path;
}

function matchesFilters(image: ImageRow, filters: LibraryFilters, exclude: Facet): boolean {
  if (exclude !== "folders" && filters.folderIds.length > 0) {
    const matched = filters.folderIds.some((folderId) => {
      const path = folderPathOf(folderId);
      return path != null && isSameOrDescendant(image.folderPath, path);
    });
    if (!matched) return false;
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

function sortValue(image: ImageRow, key: SortKey, query: LibraryQuery): number | string {
  switch (key) {
    case "importedAt":
      return image.importedAt;
    case "modifiedAt":
      return image.modifiedAt;
    case "score":
      return image.score;
    case "size":
      return image.fileSize;
    case "pixels":
      return image.width * image.height;
    case "folder":
      return image.folderPath;
    case "collectionPosition": {
      const collectionId = query.collectionId ?? -1;
      return (collectionItems.get(collectionId) ?? []).indexOf(image.id);
    }
  }
}

function compareSortValues(left: number | string, right: number | string): number {
  if (typeof left === "number" && typeof right === "number") return left - right;
  return String(left).localeCompare(String(right), "zh-Hans-CN");
}

function imageSortValues(image: ImageRow, query: LibraryQuery): (number | string)[] {
  return query.sort.map((spec) => sortValue(image, spec.key, query));
}

export function compareImages(a: ImageRow, b: ImageRow, query: LibraryQuery): number {
  for (const spec of query.sort) {
    const difference = compareSortValues(
      sortValue(a, spec.key, query),
      sortValue(b, spec.key, query),
    );
    if (difference !== 0) return difference * (spec.descending ? -1 : 1);
  }
  const last = query.sort[query.sort.length - 1];
  return (a.id - b.id) * (last?.descending ? -1 : 1);
}

function usesIdCursor(query: LibraryQuery): boolean {
  return query.sort.length > 1 || query.sort[0]?.key === "folder";
}

function orderedMatches(query: LibraryQuery): ImageRow[] {
  return mockImages
    .filter((image) => matches(image, query, null))
    .sort((a, b) => compareImages(a, b, query));
}

const idPageLists = new Map<string, number[]>();
let nextIdPage = 1;

function parsePageToken(token: string): { listId: string; offset: number } {
  const index = token.lastIndexOf(":");
  if (index < 0) throw new Error("分页 token 无效");
  return { listId: token.slice(0, index), offset: Number(token.slice(index + 1)) || 0 };
}

function pageFromIds(query: LibraryQuery, limit: number): ImagePage {
  const ordered = orderedMatches(query);
  let list: number[];
  let listId: string;
  let offset = 0;
  if (query.pageToken) {
    const parsed = parsePageToken(query.pageToken);
    const cached = idPageLists.get(parsed.listId);
    if (!cached) throw new Error("排序结果已过期，请重新载入");
    list = cached;
    listId = parsed.listId;
    offset = parsed.offset;
  } else {
    list = ordered.map((image) => image.id);
    listId = String(nextIdPage++);
    idPageLists.set(listId, list);
    while (idPageLists.size > 2) {
      const oldest = idPageLists.keys().next().value;
      if (oldest == null) break;
      idPageLists.delete(oldest);
    }
    if (query.anchorId != null) {
      const position = list.indexOf(query.anchorId);
      if (position < 0) {
        return { items: [], nextCursor: null, nextPageToken: null };
      }
      offset = position;
    }
  }
  const byId = new Map(ordered.map((image) => [image.id, image]));
  const end = Math.min(list.length, offset + limit);
  const items = list
    .slice(offset, end)
    .map((id) => byId.get(id))
    .filter((image): image is ImageRow => image != null);
  return {
    items,
    nextCursor: null,
    nextPageToken: end < list.length ? `${listId}:${end}` : null,
  };
}

export function mockQueryPage(query: LibraryQuery): ImagePage {
  const limit = Math.min(query.limit ?? 150, 300);
  if (usesIdCursor(query) || query.pageToken || query.anchorId != null) {
    return pageFromIds(query, limit);
  }
  const ordered = orderedMatches(query);
  let start = 0;
  if (query.cursor) {
    const position = ordered.findIndex((image) => image.id === query.cursor?.id);
    start = position < 0 ? ordered.length : position + 1;
  }
  const items = ordered.slice(start, start + limit);
  const last = items[items.length - 1];
  const nextCursor: QueryCursor | null =
    items.length === limit
      ? {
          sortValues: imageSortValues(last, query).map(String),
          id: last.id,
        }
      : null;
  return { items, nextCursor, nextPageToken: null };
}

export function mockQueryIds(query: LibraryQuery): number[] {
  return orderedMatches(query).map((image) => image.id);
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
  const matchedForFolders = mockImages.filter((image) => matches(image, query, "folders"));
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

  const folders: FolderCount[] = folderState
    .map((folder) => ({
      folderId: folder.id,
      count: matchedForFolders.filter((image) =>
        isSameOrDescendant(image.folderPath, folder.path),
      ).length,
    }))
    .filter((item) => item.count > 0)
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

function normalizeMockFolder(value: string): string {
  return value.replaceAll("\\", "/").trim().replace(/^\/+|\/+$/g, "");
}

function findFolder(id: number): { id: number; path: string; sortIndex: number } {
  const folder = folderState.find((item) => item.id === id);
  if (!folder) throw new Error("文件夹不存在");
  return folder;
}

function validateFolderName(name: string): string {
  const trimmed = name.trim();
  if (!trimmed) throw new Error("文件夹名称不能为空");
  if (/[\\/:*?"<>|]/.test(trimmed)) {
    throw new Error("文件夹名称包含 Windows 不允许的字符");
  }
  if (/[. ]$/.test(trimmed)) {
    throw new Error("文件夹名称不能以空格或点结尾");
  }
  if (["con", "prn", "aux", "nul"].includes(trimmed.toLowerCase())) {
    throw new Error("该名称是 Windows 保留设备名");
  }
  return trimmed;
}

function ensureMockFolder(path: string): number {
  if (!path) return folderState[0].id;
  const existing = folderState.find(
    (folder) => folder.path.toLowerCase() === path.toLowerCase(),
  );
  if (existing) return existing.id;
  let id = folderState[0].id;
  let current = "";
  for (const segment of path.split("/")) {
    current = current ? `${current}/${segment}` : segment;
    const found = folderState.find(
      (folder) => folder.path.toLowerCase() === current.toLowerCase(),
    );
    if (found) {
      id = found.id;
      continue;
    }
    const parent = parentOf(current);
    const sortIndex =
      Math.max(
        -1,
        ...folderState
          .filter((folder) => parentOf(folder.path) === parent)
          .map((folder) => folder.sortIndex),
      ) + 1;
    const folder = { id: nextFolderId++, path: current, sortIndex };
    folderState.push(folder);
    id = folder.id;
  }
  return id;
}

function replaceFolderPrefix(oldPath: string, newPath: string) {
  for (const folder of folderState) {
    if (folder.path === oldPath) folder.path = newPath;
    else if (folder.path.startsWith(`${oldPath}/`)) {
      folder.path = `${newPath}${folder.path.slice(oldPath.length)}`;
    }
  }
  for (const image of mockImages) {
    if (image.folderPath === oldPath || image.folderPath.startsWith(`${oldPath}/`)) {
      image.folderPath = `${newPath}${image.folderPath.slice(oldPath.length)}`;
      const folder = folderState.find((item) => item.path === image.folderPath);
      if (folder) image.folderId = folder.id;
      image.filePath = filePathOf(image.contentHash, image.fileExt, image.folderPath);
      image.modifiedAt = Date.now();
    }
  }
}

export function mockCreateFolder(path: string): FolderInfo[] {
  const normalized = normalizeMockFolder(path);
  if (!normalized) throw new Error("根目录已经存在");
  const segments = normalized.split("/");
  segments.forEach(validateFolderName);
  if (folderState.some((folder) => folder.path.toLowerCase() === normalized.toLowerCase())) {
    throw new Error("文件夹已存在");
  }
  ensureMockFolder(normalized);
  return mockFolders();
}

export function mockRenameFolder(id: number, name: string): FolderInfo[] {
  const folder = findFolder(id);
  if (!folder.path) throw new Error("不能重命名图库根目录");
  const nextName = validateFolderName(name);
  const index = folder.path.lastIndexOf("/");
  const parent = index < 0 ? "" : folder.path.slice(0, index);
  const nextPath = parent ? `${parent}/${nextName}` : nextName;
  if (nextPath === folder.path) return mockFolders();
  if (
    folderState.some(
      (item) => item.id !== id && item.path.toLowerCase() === nextPath.toLowerCase(),
    )
  ) {
    throw new Error("目标文件夹已存在");
  }
  replaceFolderPrefix(folder.path, nextPath);
  return mockFolders();
}

export function mockMoveFolder(id: number, parentId: number): FolderInfo[] {
  const folder = findFolder(id);
  const parent = findFolder(parentId);
  if (!folder.path) throw new Error("不能移动图库根目录");
  if (parent.path === folder.path || parent.path.startsWith(`${folder.path}/`)) {
    throw new Error("不能把文件夹移动到自身或子文件夹");
  }
  const leaf = folder.path.slice(folder.path.lastIndexOf("/") + 1);
  const nextPath = parent.path ? `${parent.path}/${leaf}` : leaf;
  if (nextPath === folder.path) return mockFolders();
  if (
    folderState.some(
      (item) => item.id !== id && item.path.toLowerCase() === nextPath.toLowerCase(),
    )
  ) {
    throw new Error("目标位置已经有同名文件夹");
  }
  replaceFolderPrefix(folder.path, nextPath);
  assignSortIndexAtEnd(id);
  return mockFolders();
}

function assignSortIndexAtEnd(id: number) {
  const folder = findFolder(id);
  const parent = parentOf(folder.path);
  folder.sortIndex =
    Math.max(
      -1,
      ...folderState
        .filter((item) => item.id !== id && parentOf(item.path) === parent)
        .map((item) => item.sortIndex),
    ) + 1;
}

export function mockReorderFolders(
  ids: number[],
  parentId: number,
  sortIndex: number,
): FolderInfo[] {
  const parent = findFolder(parentId);
  const siblings = folderState
    .filter((folder) => folder.path !== "" && parentOf(folder.path) === parent.path)
    .sort((left, right) => left.sortIndex - right.sortIndex || left.id - right.id);
  if (ids.some((id) => !siblings.some((folder) => folder.id === id))) {
    throw new Error("只能调整同一级文件夹的顺序");
  }
  const moved = new Set(ids);
  const ordered = siblings.filter((folder) => !moved.has(folder.id)).map((folder) => folder.id);
  const at = Math.max(0, Math.min(ordered.length, sortIndex));
  ordered.splice(at, 0, ...ids);
  const byId = new Map(folderState.map((folder) => [folder.id, folder]));
  ordered.forEach((id, index) => {
    const folder = byId.get(id);
    if (folder) folder.sortIndex = index;
  });
  return mockFolders();
}

export function mockDeleteFolder(id: number): FolderInfo[] {
  const folder = findFolder(id);
  if (!folder.path) throw new Error("不能删除图库根目录");
  if (folderState.some((item) => item.path.startsWith(`${folder.path}/`))) {
    throw new Error("文件夹还包含子文件夹，不能删除");
  }
  if (mockImages.some((image) => image.folderId === id)) {
    throw new Error("文件夹中还有图片，不能删除");
  }
  const index = folderState.findIndex((item) => item.id === id);
  folderState.splice(index, 1);
  return mockFolders();
}

export function mockMoveImages(ids: number[], folderPath: string): MoveReport {
  const normalized = normalizeMockFolder(folderPath);
  const folderId = ensureMockFolder(normalized);
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
  return { deleted: targetIds.size, filesRemoved: 0, deferred: true, failures: [] };
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

let mockOperations: HistoryEntry[] = [
  {
    id: 3,
    kind: "score",
    label: "设置评分",
    createdAt: Date.now() - 60_000,
    affected: 4,
    status: "applied",
    error: "",
  },
  {
    id: 2,
    kind: "tagAdd",
    label: "添加标签",
    createdAt: Date.now() - 3_600_000,
    affected: 12,
    status: "applied",
    error: "",
  },
  {
    id: 1,
    kind: "trash",
    label: "移入回收站",
    createdAt: Date.now() - 7_200_000,
    affected: 2,
    status: "applied",
    error: "",
  },
];

function mockHistoryTask(kind: TaskKind, label: string, scope: string): TaskSnapshot {
  return {
    taskId: crypto.randomUUID(),
    kind,
    label,
    scope,
    lane: "write",
    phase: "running",
    total: 0,
    completed: 0,
    failed: 0,
    skipped: 0,
    message: "",
    currentImageId: null,
    startedAt: Date.now(),
    finishedAt: null,
    failures: [],
    cancellable: false,
  };
}

export function mockOperationHistory(): HistoryPage {
  const entries = [...mockOperations].sort((left, right) => right.id - left.id);
  return {
    entries,
    maxSteps: 100,
    pendingPurgeCount: 0,
    pendingPurgeBytes: 0,
    canUndo: entries.length > 0,
    blockedReason: entries.length > 0 ? null : "还没有操作记录",
  };
}

export function mockUndoToOperation(targetId: number): TaskStartResult {
  let count = 0;
  mockOperations = mockOperations.map((entry) => {
    if (entry.id >= targetId && entry.status === "applied") {
      count += 1;
      return { ...entry, status: "undone", error: "" };
    }
    return entry;
  });
  return {
    status: "started",
    task: mockHistoryTask(
      "undo",
      "撤销操作",
      `撤销 ${Math.max(1, count)} 条记录`,
    ),
  };
}

export function mockRedoToOperation(targetId: number): TaskStartResult {
  const candidates = mockOperations
    .filter((entry) => entry.id <= targetId)
    .sort((left, right) => right.id - left.id);
  const ids = new Set<number>();
  for (const entry of candidates) {
    if (entry.status !== "undone" && entry.status !== "redoing") break;
    ids.add(entry.id);
  }
  mockOperations = mockOperations.map((entry) =>
    ids.has(entry.id) ? { ...entry, status: "applied", error: "" } : entry,
  );
  return {
    status: "started",
    task: mockHistoryTask(
      "redo",
      "重做操作",
      `重做 ${Math.max(1, ids.size)} 条记录`,
    ),
  };
}

export function mockClearOperationHistory(): TaskStartResult {
  mockOperations = [];
  return {
    status: "started",
    task: mockHistoryTask("releaseGarbage", "释放删除文件", "清空操作记录"),
  };
}
