import { useEffect, useRef } from "react";
import {
  CheckSquare,
  Copy,
  Eye,
  Hash,
  Image as ImageIcon,
  Layers,
  Palette,
  PersonStanding,
  RotateCcw,
  ScanSearch,
  Sparkles,
  Trash2,
} from "lucide-react";
import { MenuSection } from "../../shared/components/MenuSection";
import type { ImageRow } from "./types";
import type { SimilarKind } from "../../shared/similarity";

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
  onFindSimilar: (kind: SimilarKind) => void;
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
const CONTEXT_MENU_MAX_HEIGHT = 600;

export function LibraryContextMenu({
  menu,
  inCollection,
  hasActive,
  hasDeleted,
  onClose,
  onOpenViewer,
  onFindSimilar,
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
  // onClose 常是内联箭头函数：放进 ref，避免父组件每次重渲染都重装监听、把焦点抢回第一项。
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    const close = () => onCloseRef.current();
    const onPointerDown = (event: PointerEvent) => {
      if (!menuRef.current?.contains(event.target as Node)) close();
    };
    // 只跟着“用户主动滚动”关闭：程序化改 scrollTop（列数变化、锚点补偿）不算，
    // 否则右键菜单会在弹出的同一帧里被自己关掉。
    const onWheel = () => close();
    const onResize = () => close();
    document.addEventListener("pointerdown", onPointerDown);
    window.addEventListener("wheel", onWheel, { passive: true, capture: true });
    window.addEventListener("resize", onResize);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      window.removeEventListener("wheel", onWheel, { capture: true });
      window.removeEventListener("resize", onResize);
    };
  }, []);

  // 键盘导航：↑↓ 在可点项之间移动（跳过分组标题），Esc 关闭。
  useEffect(() => {
    const items = () =>
      Array.from(
        menuRef.current?.querySelectorAll<HTMLButtonElement>(".context-menu-item:not(:disabled)") ??
          [],
      );
    items()[0]?.focus({ preventScroll: true });
    const onKeyDown = (event: KeyboardEvent) => {
      const list = items();
      if (list.length === 0) return;
      const index = list.indexOf(document.activeElement as HTMLButtonElement);
      if (event.key === "ArrowDown") {
        event.preventDefault();
        list[(index + 1) % list.length]?.focus();
      } else if (event.key === "ArrowUp") {
        event.preventDefault();
        list[(index - 1 + list.length) % list.length]?.focus();
      } else if (event.key === "Home") {
        event.preventDefault();
        list[0]?.focus();
      } else if (event.key === "End") {
        event.preventDefault();
        list[list.length - 1]?.focus();
      } else if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        onCloseRef.current();
      }
    };
    document.addEventListener("keydown", onKeyDown, true);
    return () => document.removeEventListener("keydown", onKeyDown, true);
  }, []);

  const left = Math.min(menu.x, Math.max(8, window.innerWidth - CONTEXT_MENU_MAX_WIDTH));
  const top = Math.min(menu.y, Math.max(8, window.innerHeight - CONTEXT_MENU_MAX_HEIGHT));
  const multiple = menu.items.length > 1 ? menu.items.length : null;
  const canFindSimilar = menu.items[0]?.deletedAt == null;

  return (
    <div
      ref={menuRef}
      className="context-menu"
      style={{ left, top }}
      onContextMenu={(event) => event.preventDefault()}
    >
      <MenuSection title="打开">
        <button type="button" className="context-menu-item" onClick={onOpenViewer}>
          <Eye size={14} />
          <span>打开查看器</span>
        </button>
      </MenuSection>

      <MenuSection title="复制">
        <button type="button" className="context-menu-item" onClick={onCopyPaths}>
          <Copy size={14} />
          <span>文件路径</span>
          {multiple ? <em>{multiple}</em> : null}
        </button>
        <button type="button" className="context-menu-item" onClick={onCopyHashes}>
          <Hash size={14} />
          <span>内容哈希</span>
          {multiple ? <em>{multiple}</em> : null}
        </button>
        <button type="button" className="context-menu-item" onClick={onCopyImage}>
          <ImageIcon size={14} />
          <span>图片</span>
          {multiple ? <em>首张</em> : null}
        </button>
      </MenuSection>

      <MenuSection title="整理">
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
      </MenuSection>

      {canFindSimilar ? (
        <MenuSection title="找相似" aside={multiple ? "首张" : null}>
          <button
            type="button"
            className="context-menu-item"
            onClick={() => onFindSimilar("phash")}
          >
            <ScanSearch size={14} />
            <span>pHash</span>
          </button>
          <button
            type="button"
            className="context-menu-item"
            onClick={() => onFindSimilar("color8")}
          >
            <Palette size={14} />
            <span>主色（8 色）</span>
          </button>
          <button
            type="button"
            className="context-menu-item"
            onClick={() => onFindSimilar("color4")}
          >
            <Palette size={14} />
            <span>主色（4 色）</span>
          </button>
          <button
            type="button"
            className="context-menu-item"
            onClick={() => onFindSimilar("embedding")}
          >
            <Sparkles size={14} />
            <span>语义</span>
          </button>
          <button
            type="button"
            className="context-menu-item"
            onClick={() => onFindSimilar("pose")}
          >
            <PersonStanding size={14} />
            <span>姿势</span>
          </button>
        </MenuSection>
      ) : null}

      {hasActive || hasDeleted ? (
        <MenuSection title="危险操作">
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
        </MenuSection>
      ) : null}

      <MenuSection title="选择">
        <button type="button" className="context-menu-item" onClick={onSelectAllMatches}>
          <CheckSquare size={14} />
          <span>全部匹配项</span>
        </button>
      </MenuSection>
    </div>
  );
}
