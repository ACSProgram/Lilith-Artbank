import { useEffect, useRef, useState } from "react";
import {
  ArrowLeft,
  ArrowRight,
  Copy,
  Images,
  Layers,
  MoreHorizontal,
  Pencil,
  Plus,
  Timer,
  Trash2,
} from "lucide-react";
import { Button } from "../../shared/components/Button";
import { Popover } from "../../shared/components/Popover";
import type { CollectionInfo, LibraryView } from "./types";

interface ViewBarProps {
  view: LibraryView;
  collectionId: number | null;
  collections: CollectionInfo[];
  imageCount: number;
  trashCount: number;
  tempCount: number;
  emptyingTrash: boolean;
  onShowAll: () => void;
  onShowTrash: () => void;
  onShowTemp: () => void;
  onShowCollection: (collectionId: number) => void;
  onNewCollection: () => void;
  onManageCollections: () => void;
  onCombineCollections: () => void;
  onRenameCollection: (collection: CollectionInfo) => void;
  onDeleteCollection: (collection: CollectionInfo) => void;
  onMoveCollection: (collection: CollectionInfo, delta: number) => void;
  /** 以该集合的内容新建一个集合。 */
  onDuplicateCollection: (collection: CollectionInfo) => void;
  onEmptyTrash: () => void;
}

interface TabMenuState {
  x: number;
  y: number;
  collection: CollectionInfo;
}

/** 视图栏：全部 / 回收站两个固定视图、集合胶囊、新建集合与集合菜单。 */
export function ViewBar({
  view,
  collectionId,
  collections,
  imageCount,
  trashCount,
  tempCount,
  emptyingTrash,
  onShowAll,
  onShowTrash,
  onShowTemp,
  onShowCollection,
  onNewCollection,
  onManageCollections,
  onCombineCollections,
  onRenameCollection,
  onDeleteCollection,
  onMoveCollection,
  onDuplicateCollection,
  onEmptyTrash,
}: ViewBarProps) {
  const moreRef = useRef<HTMLButtonElement>(null);
  const [menuOpen, setMenuOpen] = useState(false);
  const [tabMenu, setTabMenu] = useState<TabMenuState | null>(null);

  useEffect(() => {
    if (!tabMenu) return;
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as HTMLElement;
      if (!target.closest(".view-tab-menu")) setTabMenu(null);
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [tabMenu]);

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
      <button
        type="button"
        role="tab"
        aria-selected={view === "temp"}
        className={`view-tab view-tab-temp${view === "temp" ? " is-active" : ""}`}
        title="临时区：关闭、切换图库或退出应用时自动清空"
        onClick={onShowTemp}
      >
        <Timer size={14} />
        <span>临时</span>
        <em>{tempCount.toLocaleString()}</em>
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
            title={`集合 · ${collection.name}（右键更多操作）`}
            onClick={() => onShowCollection(collection.id)}
            onContextMenu={(event) => {
              event.preventDefault();
              setTabMenu({ x: event.clientX, y: event.clientY, collection });
            }}
          >
            <Layers size={13} />
            <span>{collection.name}</span>
            <em>{collection.imageCount.toLocaleString()}</em>
          </button>
        ))}
      </div>

      <button
        type="button"
        className="view-tab view-tab-add"
        disabled={view === "temp"}
        title={view === "temp" ? "临时图片不能加入集合" : undefined}
        onClick={onNewCollection}
      >
        <Plus size={13} />
        <span>新建集合</span>
      </button>

      <button
        ref={moreRef}
        type="button"
        className="view-bar-icon"
        title="集合管理"
        aria-label="集合管理"
        aria-expanded={menuOpen}
        onClick={() => setMenuOpen((open) => !open)}
      >
        <MoreHorizontal size={15} />
      </button>
      <Popover open={menuOpen} anchorRef={moreRef} onClose={() => setMenuOpen(false)} minWidth={168}>
        <button
          type="button"
          className="context-menu-item"
          onClick={() => {
            setMenuOpen(false);
            onManageCollections();
          }}
        >
          <Layers size={14} />
          <span>管理集合…</span>
        </button>
        <button
          type="button"
          className="context-menu-item"
          disabled={collections.length < 2}
          title={collections.length < 2 ? "至少需要两个集合" : undefined}
          onClick={() => {
            setMenuOpen(false);
            onCombineCollections();
          }}
        >
          <Plus size={14} />
          <span>集合运算…</span>
        </button>
      </Popover>

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

      {tabMenu ? (
        <div
          className="context-menu view-tab-menu"
          style={{
            left: Math.min(tabMenu.x, Math.max(8, window.innerWidth - 240)),
            top: Math.min(tabMenu.y, Math.max(8, window.innerHeight - 200)),
          }}
        >
          <button
            type="button"
            className="context-menu-item"
            onClick={() => {
              onRenameCollection(tabMenu.collection);
              setTabMenu(null);
            }}
          >
            <Pencil size={14} />
            <span>重命名</span>
          </button>
          <button
            type="button"
            className="context-menu-item"
            disabled={collections.findIndex((item) => item.id === tabMenu.collection.id) <= 0}
            onClick={() => {
              onMoveCollection(tabMenu.collection, -1);
              setTabMenu(null);
            }}
          >
            <ArrowLeft size={14} />
            <span>左移</span>
          </button>
          <button
            type="button"
            className="context-menu-item"
            disabled={
              collections.findIndex((item) => item.id === tabMenu.collection.id) >=
              collections.length - 1
            }
            onClick={() => {
              onMoveCollection(tabMenu.collection, 1);
              setTabMenu(null);
            }}
          >
            <ArrowRight size={14} />
            <span>右移</span>
          </button>
          <div className="context-menu-separator" />
          <button
            type="button"
            className="context-menu-item"
            onClick={() => {
              onDuplicateCollection(tabMenu.collection);
              setTabMenu(null);
            }}
          >
            <Copy size={14} />
            <span>另存为新集合…</span>
          </button>
          <button
            type="button"
            className="context-menu-item context-menu-danger"
            onClick={() => {
              onDeleteCollection(tabMenu.collection);
              setTabMenu(null);
            }}
          >
            <Trash2 size={14} />
            <span>删除集合</span>
          </button>
        </div>
      ) : null}
    </div>
  );
}
