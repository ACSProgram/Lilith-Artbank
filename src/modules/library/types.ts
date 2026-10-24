export type LibraryView = "all" | "trash" | "collection";
export type SortKey = "importedAt" | "score" | "size" | "collectionPosition";
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
  sortKey: string;
  id: number;
}

export interface LibraryQuery {
  view: LibraryView;
  collectionId?: number | null;
  search: string;
  searchScope: SearchScope;
  filters: LibraryFilters;
  sort: SortSpec;
  cursor?: QueryCursor | null;
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
  failures: BatchFailure[];
}

export interface CollectionInfo {
  id: number;
  name: string;
  sortOrder: number;
  imageCount: number;
  createdAt: number;
  updatedAt: number;
}
