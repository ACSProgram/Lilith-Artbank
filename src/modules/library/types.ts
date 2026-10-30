export type LibraryView = "all" | "trash" | "collection" | "temp";
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
  /** 选中文件夹只匹配直接位于其中的图片，不包含子文件夹。 */
  folderDirect: boolean;
  /** 回收站根行的“直属图片”：只匹配原文件夹仍在图库中的回收站图片。 */
  trashLoose: boolean;
  extensions: string[];
  scores: number[];
  tagIds: number[];
  tagMode: TagMode;
  /** 标签存在性；missing 表示“一个标签都没有”，此时 tagIds 必须为空。 */
  tagPresence: PresenceFilter;
  /** 排除拥有其中任一标签的图片。 */
  excludeTagIds: number[];
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
  /** 所在文件夹已整目录移入回收站；这类图片不提供单张恢复/永久删除。 */
  folderTrashed: boolean;
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
  extensions: ValueCount[];
  scores: ValueCount[];
  tags: TagCount[];
}

export type StatsProfile = "facets" | "summary" | "folderDetail";

export interface StatsRequest {
  profile: StatsProfile;
  query: LibraryQuery;
}

export interface SummaryStats {
  total: number;
}

export interface StatsTag {
  id: number;
  name: string;
  count: number;
}

export interface FolderStats {
  imageCount: number;
  totalBytes: number;
  subfolderCount: number;
  extensions: ValueCount[];
  scores: ValueCount[];
  tags: StatsTag[];
}

export type StatsSnapshot =
  | { profile: "facets"; data: FilterCounts }
  | { profile: "summary"; data: SummaryStats }
  | { profile: "folderDetail"; data: FolderStats };

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

/** 文件夹写命令的增量结果：只含受影响的行，前端按 id 就地合并。 */
export interface FolderChange {
  folders: FolderInfo[];
  removedIds: number[];
  /** 这次操作涉及的图片数；其它文件夹写命令为 0。 */
  affectedImages: number;
}

/** 回收站文件夹树；计数都是回收站图片数。 */
export interface TrashFolders {
  folders: FolderInfo[];
  /** 原文件夹仍在图库中的回收站图片数（“直属图片”行）。 */
  directImageCount: number;
  totalImageCount: number;
}

export type FolderSortKey = "name" | "total";

export interface FolderOrder {
  parentId: number;
  folderIds: number[];
}

export interface TagInfo {
  id: number;
  name: string;
  imageCount: number;
}

/** 合并两个标签的结果：源标签的图片全部并到目标标签后，源标签被删除。 */
export interface TagMergeReport {
  sourceImages: number;
  moved: number;
  duplicate: number;
  sourceName: string;
  targetName: string;
}

export interface TagRef {
  id: number;
  name: string;
}

/** 选中集合内某个标签的拥有数；不是全库数量。 */
export interface SelectionTag {
  id: number;
  name: string;
  count: number;
}

export interface SelectionTags {
  /** 去重后的选中图片数，用于判断“全部都有”。 */
  total: number;
  tags: SelectionTag[];
}

export interface ColorEntry {
  r: number;
  g: number;
  b: number;
  weight: number;
}

/** 外部变化后复核用；已删除的图片也会返回，找不到的 id 直接缺席。 */
export interface ImageState {
  id: number;
  deletedAt: number | null;
}

export interface ImageDetails {
  id: number;
  contentHash: string;
  fileExt: string;
  filePath: string;
  folderId: number;
  folderPath: string;
  /** 所在文件夹已整目录移入回收站；这类图片不提供单张恢复/永久删除。 */
  folderTrashed: boolean;
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
  | "folderTrash"
  | "folderRestore"
  | "folderPurge"
  | "score"
  | "description"
  | "tagSet"
  | "tagAdd"
  | "tagRemove"
  | "tagRename"
  | "tagDelete"
  | "tagMerge"
  | "collectionCreate"
  | "collectionRename"
  | "collectionDelete"
  | "collectionReorder"
  | "collectionAdd"
  | "collectionRemove"
  | "collectionCombine"
  | "similarityGroups"
  | "aiDescription"
  | "optimize";

export type HistoryStatus =
  | "applied"
  | "undone"
  | "redoing"
  | "undoFailed"
  /** 用户放弃撤销失败的记录：保留在时间线里，不再参与撤销与重做。 */
  | "undoSkipped";

export interface HistoryEntry {
  id: number;
  kind: HistoryKind;
  label: string;
  createdAt: number;
  affected: number;
  status: HistoryStatus;
  error: string;
}

export interface HistoryStorage {
  recordCount: number;
  chunkCount: number;
  chunkBytes: number;
  undoFiles: number;
  undoBytes: number;
  totalBytes: number;
}

export interface HistoryPage {
  entries: HistoryEntry[];
  maxSteps: number;
  pendingPurgeCount: number;
  pendingPurgeBytes: number;
  storage: HistoryStorage;
  /** 上次撤销/重做异常中断，且此刻没有对应任务在跑。 */
  recoveryNeeded: boolean;
  /** 此刻能否开始撤销或重做。 */
  canRun: boolean;
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
