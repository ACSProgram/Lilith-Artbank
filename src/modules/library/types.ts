export type LibraryView = "all" | "trash" | "collection";
export type SortKey =
  | "importedAt"
  | "modifiedAt"
  | "score"
  | "size"
  | "pixels"
  | "folder"
  | "collectionPosition";
export type TagMode = "any" | "all";
export type ViewMode = "grid" | "list";
export type SearchScope = "view" | "library";
export type PresenceFilter = "any" | "has" | "missing";
export type CombineMode = "union" | "intersection" | "difference";

export interface LibraryFilters {
  folderIds: number[];
  extensions: string[];
  scores: number[];
  tagIds: number[];
  tagMode: TagMode;
  importedFrom: number | null;
  importedTo: number | null;
  sizeMin: number | null;
  sizeMax: number | null;
  description: PresenceFilter;
  embedding: PresenceFilter;
  pose: PresenceFilter;
}

export interface SortSpec {
  key: SortKey;
  descending: boolean;
}

export interface QueryCursor {
  sortValues: string[];
  id: number;
}

export interface LibraryQuery {
  view: LibraryView;
  collectionId?: number | null;
  search: string;
  searchScope: SearchScope;
  filters: LibraryFilters;
  /** 第一级为主排序；最多 7 级，指标不可重复。 */
  sort: SortSpec[];
  /** 单级排序的键集游标。 */
  cursor?: QueryCursor | null;
  /** 多级或文件夹排序时的有序 id 列表游标。 */
  pageToken?: string | null;
  /** 从这张图所在的排序位置开始取一页（包含它自身）。 */
  anchorId?: number | null;
  limit?: number | null;
}

export interface ImageRow {
  id: number;
  contentHash: string;
  fileExt: string;
  folderId: number;
  folderPath: string;
  filePath: string;
  fileSize: number;
  width: number;
  height: number;
  format: string;
  hasAlpha: boolean;
  score: number;
  importedAt: number;
  modifiedAt: number;
  deletedAt: number | null;
  thumbVersion: number;
  thumbWidth: number;
  thumbHeight: number;
  tags: string[];
}

export interface ImagePage {
  items: ImageRow[];
  nextCursor: QueryCursor | null;
  nextPageToken: string | null;
}

export interface FolderCount {
  folderId: number;
  count: number;
}

export interface ValueCount {
  value: string;
  count: number;
}

export interface TagCount {
  tagId: number;
  count: number;
}

export interface FilterCounts {
  total: number;
  folders: FolderCount[];
  extensions: ValueCount[];
  scores: ValueCount[];
  tags: TagCount[];
}

export interface FolderInfo {
  id: number;
  path: string;
  /** 同一父级下的手动排序位。 */
  sortIndex: number;
  /** 直接位于该文件夹的未删除图片数。 */
  directImageCount: number;
  /** 包含全部后代的未删除图片数。 */
  totalImageCount: number;
}

export interface TagInfo {
  id: number;
  name: string;
  imageCount: number;
}

export interface TagRef {
  id: number;
  name: string;
}

export interface ColorEntry {
  r: number;
  g: number;
  b: number;
  weight: number;
}

export interface ImageDetails {
  id: number;
  contentHash: string;
  fileExt: string;
  filePath: string;
  folderId: number;
  folderPath: string;
  fileSize: number;
  width: number;
  height: number;
  format: string;
  hasAlpha: boolean;
  score: number;
  description: string;
  descriptionSource: string;
  descriptionModel: string;
  importedAt: number;
  modifiedAt: number;
  deletedAt: number | null;
  thumbVersion: number;
  thumbWidth: number;
  thumbHeight: number;
  tags: TagRef[];
  colors4: ColorEntry[];
  colors8: ColorEntry[];
}

export interface BatchFailure {
  imageId: number | null;
  path: string;
  reason: string;
}

export interface MoveReport {
  moved: number;
  skipped: number;
  failures: BatchFailure[];
}

export interface DeleteReport {
  deleted: number;
  filesRemoved: number;
  /** 永久删除/清空回收站只做标记；文件在记录过期或清空记录后释放。 */
  deferred: boolean;
  failures: BatchFailure[];
}

export type HistoryKind =
  | "import"
  | "edit"
  | "move"
  | "trash"
  | "restore"
  | "permanentDelete"
  | "emptyTrash"
  | "folderCreate"
  | "folderRename"
  | "folderMove"
  | "folderDelete"
  | "folderReorder"
  | "score"
  | "description"
  | "tagSet"
  | "tagAdd"
  | "tagRemove"
  | "tagRename"
  | "tagDelete"
  | "collectionCreate"
  | "collectionRename"
  | "collectionDelete"
  | "collectionReorder"
  | "collectionAdd"
  | "collectionRemove"
  | "collectionCombine"
  | "similarityGroups"
  | "aiDescription";

export type HistoryStatus = "applied" | "undone" | "redoing" | "undoFailed";

export interface HistoryEntry {
  id: number;
  kind: HistoryKind;
  label: string;
  createdAt: number;
  affected: number;
  status: HistoryStatus;
  error: string;
}

export interface HistoryPage {
  entries: HistoryEntry[];
  maxSteps: number;
  pendingPurgeCount: number;
  pendingPurgeBytes: number;
  canUndo: boolean;
  blockedReason: string | null;
}

export interface CollectionInfo {
  id: number;
  name: string;
  sortOrder: number;
  imageCount: number;
  createdAt: number;
  updatedAt: number;
}

/** 单张图片具备哪些可比较特征；只在右键菜单/详情里按需查询。 */
export interface ImageCapabilities {
  imageId: number;
  pose: boolean;
  embedding: boolean;
  colors: boolean;
  phash: boolean;
}

/** 全库特征覆盖情况，用于筛选面板与工具栏置灰。 */
export interface FeatureSummary {
  description: number;
  embedding: number;
  pose: number;
  phash: number;
  colors: number;
}
