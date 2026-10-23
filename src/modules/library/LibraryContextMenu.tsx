import { useEffect, useRef } from "react";
import {
  CheckSquare,
  Copy,
  Eye,
  Hash,
  Image as ImageIcon,
  Layers,
  PanelRight,
  RotateCcw,
  Trash2,
} from "lucide-react";
import type { ImageRow } from "./types";

export interface ContextMenuState {
  x: number;
  y: number;
  /** 菜单里能逐项操作的图片：只有当前已加载的行。 */
  items: ImageRow[];
  /** 实际执行的目标 id：可能包含尚未滚动加载的选中项。 */
  ids: number[];
}

interface LibraryContextMenuProps {
  menu: ContextMenuState;
  inCollection: boolean;
  hasActive: boolean;
  hasDeleted: boolean;
  onClose: () => void;
  onOpenViewer: () => void;
  onShowDetails: () => void;
  onCopyPaths: () => void;
  onCopyHashes: () => void;
  onCopyImage: () => void;
  onAddToCollection: () => void;
  onRemoveFromCollection: () => void;
  onTrash: () => void;
  onRestore: () => void;
  onDelete: () => void;
  onSelectAllMatches: () => void;
}

/** 右键菜单尺寸上限，用于把菜单夹在视口内（`.context-menu` 为 min-width 214px + 内边距）。 */
const CONTEXT_MENU_MAX_WIDTH = 264;
const CONTEXT_MENU_MAX_HEIGHT = 380;

export function LibraryContextMenu({
  menu,
  inCollection,
  hasActive,
  hasDeleted,
  onClose,
  onOpenViewer,
  onShowDetails,
  onCopyPaths,
  onCopyHashes,
  onCopyImage,
  onAddToCollection,
  onRemoveFromCollection,
  onTrash,
  onRestore,
  onDelete,
  onSelectAllMatches,
}: LibraryContextMenuProps) {
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const onPointerDown = (event: PointerEvent) => {
      if (!menuRef.current?.contains(event.target as Node)) onClose();
    };
    const onScroll = () => onClose();
    document.addEventListener("pointerdown", onPointerDown);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [onClose]);

  const left = Math.min(menu.x, Math.max(8, window.innerWidth - CONTEXT_MENU_MAX_WIDTH));
  const top = Math.min(menu.y, Math.max(8, window.innerHeight - CONTEXT_MENU_MAX_HEIGHT));
  const multiple = menu.items.length > 1 ? menu.items.length : null;

  return (
    <div
      ref={menuRef}
      className="context-menu"
      style={{ left, top }}
      onContextMenu={(event) => event.preventDefault()}
    >
      <button type="button" className="context-menu-item" onClick={onOpenViewer}>
        <Eye size={14} />
        <span>打开查看器</span>
      </button>
      <button type="button" className="context-menu-item" onClick={onShowDetails}>
        <PanelRight size={14} />
        <span>在侧栏查看详情</span>
      </button>
      <button type="button" className="context-menu-item" onClick={onCopyPaths}>
        <Copy size={14} />
        <span>复制文件路径</span>
        {multiple ? <em>{multiple}</em> : null}
      </button>
      <button type="button" className="context-menu-item" onClick={onCopyHashes}>
        <Hash size={14} />
        <span>复制内容哈希</span>
        {multiple ? <em>{multiple}</em> : null}
      </button>
      <button type="button" className="context-menu-item" onClick={onCopyImage}>
        <ImageIcon size={14} />
        <span>复制图片</span>
        {multiple ? <em>首张</em> : null}
      </button>
      <button type="button" className="context-menu-item" onClick={onAddToCollection}>
        <Layers size={14} />
        <span>加入集合…</span>
      </button>
      {inCollection ? (
        <button type="button" className="context-menu-item" onClick={onRemoveFromCollection}>
          <Layers size={14} />
          <span>移出当前集合</span>
        </button>
      ) : null}
      {hasActive ? (
        <button type="button" className="context-menu-item" onClick={onTrash}>
          <Trash2 size={14} />
          <span>移入回收站</span>
        </button>
      ) : null}
      {hasDeleted ? (
        <>
          <button type="button" className="context-menu-item" onClick={onRestore}>
            <RotateCcw size={14} />
            <span>恢复</span>
          </button>
          <button
            type="button"
            className="context-menu-item context-menu-danger"
            onClick={onDelete}
          >
            <Trash2 size={14} />
            <span>永久删除</span>
          </button>
        </>
      ) : null}
      <div className="context-menu-separator" />
      <button type="button" className="context-menu-item" onClick={onSelectAllMatches}>
        <CheckSquare size={14} />
        <span>选择全部匹配项</span>
      </button>
    </div>
  );
}
