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

/** 全库特征覆盖情况，用于筛选面板与工具栏置灰。 */
export interface FeatureSummary {
  description: number;
  embedding: number;
  pose: number;
  phash: number;
  colors: number;
}
