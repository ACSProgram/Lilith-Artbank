import { Images, Layers, Plus, Trash2 } from "lucide-react";
import { Button } from "../../shared/components/Button";
import type { CollectionInfo, LibraryView } from "./types";

interface ViewBarProps {
  view: LibraryView;
  collectionId: number | null;
  collections: CollectionInfo[];
  imageCount: number;
  trashCount: number;
  emptyingTrash: boolean;
  onShowAll: () => void;
  onShowTrash: () => void;
  onShowCollection: (collectionId: number) => void;
  onManageCollections: () => void;
  onEmptyTrash: () => void;
}

/** 视图栏：全部 / 回收站两个固定视图、集合胶囊，以及集合管理与清空回收站入口。 */
export function ViewBar({
  view,
  collectionId,
  collections,
  imageCount,
  trashCount,
  emptyingTrash,
  onShowAll,
  onShowTrash,
  onShowCollection,
  onManageCollections,
  onEmptyTrash,
}: ViewBarProps) {
  return (
    <div className="view-bar" role="tablist" aria-label="图库视图">
      <button
        type="button"
        role="tab"
        aria-selected={view === "all"}
        className={`view-tab${view === "all" ? " is-active" : ""}`}
        onClick={onShowAll}
      >
        <Images size={14} />
        <span>全部</span>
        <em>{imageCount.toLocaleString()}</em>
      </button>
      <button
        type="button"
        role="tab"
        aria-selected={view === "trash"}
        className={`view-tab view-tab-trash${view === "trash" ? " is-active" : ""}`}
        onClick={onShowTrash}
      >
        <Trash2 size={14} />
        <span>回收站</span>
        <em>{trashCount.toLocaleString()}</em>
      </button>

      <span className="view-bar-separator" aria-hidden="true" />

      <div className="view-collections">
        {collections.map((collection) => (
          <button
            key={collection.id}
            type="button"
            role="tab"
            aria-selected={view === "collection" && collectionId === collection.id}
            className={`view-tab view-tab-collection${
              view === "collection" && collectionId === collection.id ? " is-active" : ""
            }`}
            title={`集合 · ${collection.name}`}
            onClick={() => onShowCollection(collection.id)}
          >
            <Layers size={13} />
            <span>{collection.name}</span>
            <em>{collection.imageCount.toLocaleString()}</em>
          </button>
        ))}
      </div>
      <button
        type="button"
        className="view-bar-icon"
        title="新建或管理集合"
        aria-label="新建或管理集合"
        onClick={onManageCollections}
      >
        <Plus size={14} />
      </button>

      <div className="view-bar-actions">
        {view === "trash" ? (
          <Button
            size="sm"
            variant="danger"
            icon={<Trash2 size={13} />}
            loading={emptyingTrash}
            disabled={trashCount === 0}
            onClick={onEmptyTrash}
          >
            清空回收站
          </Button>
        ) : null}
      </div>
    </div>
  );
}
