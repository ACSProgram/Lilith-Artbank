/** 查看器与图库浏览之间的最小契约；由 `src/app` 编排，模块之间不直接导入。 */
export interface ViewerSessionItem {
  id: number;
  width: number;
  height: number;
  contentHash: string;
  fileExt: string;
  format: string;
  fileSize: number;
  hasAlpha: boolean;
  deletedAt: number | null;
  thumbVersion: number;
}

export interface ViewerSessionFolder {
  id: number;
  path: string;
  directImageCount: number;
  totalImageCount: number;
}

export interface ViewerSession {
  initialId: number;
  folders: ViewerSessionFolder[];
  currentCollectionId: number | null;
  /** 必须返回稳定引用，供 useSyncExternalStore 使用。 */
  getItems(): readonly ViewerSessionItem[];
  hasMore(): boolean;
  subscribe(listener: () => void): () => void;
  loadMore(): void;
  onActiveChange(imageId: number): void;
  onSaved(imageId: number): void;
  refresh(): void;
  addToCollection(imageIds: number[]): void;
  removeFromCollection(imageIds: number[]): Promise<boolean>;
  trash(imageIds: number[]): Promise<boolean>;
  restore(imageIds: number[]): Promise<boolean>;
  delete(imageIds: number[]): Promise<boolean>;
}
