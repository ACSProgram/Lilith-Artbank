import { PanelRight, X } from "lucide-react";
import { IconButton } from "../../shared/components/Button";
import type { TagFilterAction, TagFilterScope } from "../../shared/tagFilter";
import type { DataChange } from "./dataChange";
import { FolderInspector } from "./FolderInspector";
import { ImageInspector } from "./ImageInspector";
import { BatchInspector } from "./BatchInspector";
import type { FolderInfo, LibraryQuery } from "./types";

export interface InspectorStatsContext {
  query: LibraryQuery;
}

interface InspectorPanelProps {
  selectedIds: number[];
  version: number;
  /** 标签关系或标签目录变化时递增；批量标签候选只在它变化时重算。 */
  tagVersion?: number;
  folders: FolderInfo[];
  currentCollectionId: number | null;
  /** 当前在临时视图：隐藏移动、集合与回收站操作。 */
  inTemp: boolean;
  /** 当前在回收站视图：回收站操作只给恢复/永久删除。 */
  inTrash: boolean;
  onClose: () => void;
  onChanged: (change: DataChange) => void;
  onAddToCollection: () => void;
  onRemoveFromCollection: (ids: number[]) => void;
  onTrash: (ids: number[]) => void;
  onRestore: (ids: number[]) => void;
  onDelete: (ids: number[]) => void;
  /** 点击标签按选择的范围筛选；Ctrl/Cmd 点击直接在当前视图追加。 */
  onFilterTag?: (tagId: number, action: TagFilterAction, scope: TagFilterScope) => void;
  filterTagIds?: number[];
  /** 当前侧栏选中的文件夹；为空表示“当前文件夹”范围不可用。 */
  folderScopeLabel?: string | null;
  /** 当前集合；为空表示“当前集合”范围不可用。 */
  collectionScopeLabel?: string | null;
  onGenerateDescription?: (imageId: number) => Promise<{ text: string; model: string }>;
  /** 跳到图片所在文件夹；不传或回收站中时隐藏入口。 */
  onOpenFolder?: (folderId: number) => void;
  /** 空选且有选中文件夹时显示文件夹概览；查看器不传。 */
  statsContext?: InspectorStatsContext | null;
}

export function InspectorPanel({
  selectedIds,
  version,
  tagVersion = 0,
  folders,
  currentCollectionId,
  inTemp,
  inTrash,
  onClose,
  onChanged,
  onAddToCollection,
  onRemoveFromCollection,
  onTrash,
  onRestore,
  onDelete,
  onFilterTag,
  filterTagIds = [],
  folderScopeLabel = null,
  collectionScopeLabel = null,
  onGenerateDescription,
  onOpenFolder,
  statsContext = null,
}: InspectorPanelProps) {
  const single = selectedIds.length === 1 ? selectedIds[0] : null;
  const empty = selectedIds.length === 0;
  // 空选与单张都是“详情”，多选才是“批量编辑”。
  const detail = empty || single != null;
  // 有文件夹条件时是文件夹概览；全部图片视图没有文件夹条件时是整库概览。
  const overviewQuery =
    empty &&
    statsContext != null &&
    (statsContext.query.filters.folderIds.length > 0 || statsContext.query.view === "all")
      ? statsContext.query
      : null;
  const folderOverview = overviewQuery != null;
  const libraryWide = overviewQuery != null && overviewQuery.filters.folderIds.length === 0;
  const title = folderOverview
    ? libraryWide
      ? "图库概览"
      : "文件夹概览"
    : detail
      ? "图片详情"
      : "批量编辑";
  return (
    <aside className="inspector" aria-label={title}>
      <header className="inspector-head">
        <div>
          <strong>{title}</strong>
          <span>
            {folderOverview
              ? libraryWide
                ? "全部图片"
                : (folderScopeLabel ?? "已选文件夹")
              : empty
                ? "未选择"
                : single
                  ? `#${single}`
                  : `已选择 ${selectedIds.length.toLocaleString()} 张`}
          </span>
        </div>
        <IconButton label="关闭侧栏" onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>
      {overviewQuery != null ? (
        <FolderInspector query={overviewQuery} />
      ) : empty ? (
        <div className="inspector-placeholder">
          <PanelRight size={22} />
          <span>点选一张图片查看详情</span>
        </div>
      ) : single ? (
        <ImageInspector
          imageId={single}
          version={version}
          folders={folders}
          currentCollectionId={currentCollectionId}
          inTemp={inTemp}
          inTrash={inTrash}
          onChanged={onChanged}
          onAddToCollection={onAddToCollection}
          onRemoveFromCollection={() => onRemoveFromCollection([single])}
          onTrash={() => onTrash([single])}
          onRestore={() => onRestore([single])}
          onDelete={() => onDelete([single])}
          onFilterTag={onFilterTag}
          filterTagIds={filterTagIds}
          folderScopeLabel={folderScopeLabel}
          collectionScopeLabel={collectionScopeLabel}
          onGenerateDescription={onGenerateDescription}
          onOpenFolder={onOpenFolder}
        />
      ) : (
        <BatchInspector
          imageIds={selectedIds}
          tagVersion={tagVersion}
          folders={folders}
          currentCollectionId={currentCollectionId}
          inTemp={inTemp}
          inTrash={inTrash}
          onChanged={onChanged}
          onAddToCollection={onAddToCollection}
          onRemoveFromCollection={() => onRemoveFromCollection(selectedIds)}
          onTrash={() => onTrash(selectedIds)}
          onRestore={() => onRestore(selectedIds)}
          onDelete={() => onDelete(selectedIds)}
        />
      )}
    </aside>
  );
}
