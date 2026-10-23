export type LibraryView = "all" | "trash";
export type SortKey = "importedAt" | "score" | "size";
export type TagMode = "any" | "all";
export type ViewMode = "grid" | "list";

export interface LibraryFilters {
  folderIds: number[];
  extensions: string[];
  scores: number[];
  tagIds: number[];
  tagMode: TagMode;
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
  imageCount: number;
}

export interface TagInfo {
  id: number;
  name: string;
  imageCount: number;
}
