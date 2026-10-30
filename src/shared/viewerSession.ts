import type { TagFilterAction, TagFilterScope } from "./tagFilter";
import type { DataChange } from "./contracts/dataChange";

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
  sortIndex: number;
  directImageCount: number;
  totalImageCount: number;
}

/** 保存编辑后需要就地替换的列表字段；不是整行重新查询。 */
export interface ViewerSessionUpdate {
  contentHash: string;
  fileExt: string;
  fileSize: number;
  width: number;
  height: number;
  format: string;
  hasAlpha: boolean;
  thumbVersion: number;
}

export interface ViewerSession {
  initialId: number;
  folders: ViewerSessionFolder[];
  currentCollectionId: number | null;
  /** 浏览的是临时文件夹：详情侧栏隐藏移动、集合与回收站操作。 */
  inTemp: boolean;
  /** 浏览的是回收站：外部恢复后当前图会离开列表。 */
  expectTrashed: boolean;
  /** 必须返回稳定引用，供 useSyncExternalStore 使用。 */
  getItems(): readonly ViewerSessionItem[];
  hasMore(): boolean;
  subscribe(listener: () => void): () => void;
  loadMore(): void;
  onActiveChange(imageId: number): void;
  onSaved(imageId: number, update: ViewerSessionUpdate): void;
  /** 详情侧栏或查看器产生的写入；交给图库统一刷新与失效。 */
  refresh(change: DataChange): void;
  /** 在网格里按标签筛选；查看器会把控制权交回网格。 */
  filterByTag(tagId: number, action: TagFilterAction, scope: TagFilterScope): void;
  /** 侧栏当前选中的文件夹（用于标签筛选范围菜单）；为空表示不可用。 */
  folderScopeLabel: string | null;
  collectionScopeLabel: string | null;
  /** 打开查看器时已经生效的标签筛选，用于高亮详情里的标签。 */
  filterTagIds: number[];
  addToCollection(imageIds: number[]): void;
  removeFromCollection(imageIds: number[]): Promise<boolean>;
  trash(imageIds: number[]): Promise<boolean>;
  restore(imageIds: number[]): Promise<boolean>;
  delete(imageIds: number[]): Promise<boolean>;
}
