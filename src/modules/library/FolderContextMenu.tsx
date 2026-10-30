import type { RefObject } from "react";
import {
  ArrowDownUp,
  Folder,
  FolderInput,
  FolderPlus,
  Pencil,
  RotateCcw,
  Trash2,
} from "lucide-react";
import type { FolderNode } from "./folderTreeModel";

export interface FolderMenu {
  x: number;
  y: number;
  /** null 表示“全部图片”根行。 */
  node: FolderNode | null;
}

interface FolderContextMenuProps {
  menu: FolderMenu;
  menuRef: RefObject<HTMLDivElement | null>;
  trashMode: boolean;
  menuIsRoot: boolean;
  sortDisabled: boolean;
  selectedImageCount: number;
  onRestore: (node: FolderNode | null) => void;
  onDeletePermanently: (node: FolderNode | null) => void;
  onCreateFolder: (parentPath: string) => void;
  onRename: (node: FolderNode | null) => void;
  onSort: () => void;
  onMove: (node: FolderNode | null) => void;
  onMoveImages: (node: FolderNode | null) => void;
  onTrash: (node: FolderNode | null) => void;
}

/** 文件夹树的右键菜单；菜单项的可执行性由父组件算好，点击回调原样交回父组件。 */
export function FolderContextMenu({
  menu,
  menuRef,
  trashMode,
  menuIsRoot,
  sortDisabled,
  selectedImageCount,
  onRestore,
  onDeletePermanently,
  onCreateFolder,
  onRename,
  onSort,
  onMove,
  onMoveImages,
  onTrash,
}: FolderContextMenuProps) {
  const menuNode = menu.node;

  return (
    <>
      {trashMode && menuNode ? (
        <div
          ref={menuRef}
          className="context-menu folder-context-menu"
          style={{
            left: Math.min(menu.x, Math.max(8, window.innerWidth - 224)),
            top: Math.min(menu.y, Math.max(8, window.innerHeight - 160)),
          }}
          onContextMenu={(event) => event.preventDefault()}
        >
          <button
            type="button"
            className="context-menu-item"
            onClick={() => onRestore(menuNode)}
          >
            <RotateCcw size={14} />
            <span>恢复文件夹</span>
          </button>
          <button
            type="button"
            className="context-menu-item context-menu-danger"
            onClick={() => onDeletePermanently(menuNode)}
          >
            <Trash2 size={14} />
            <span>永久删除文件夹</span>
          </button>
        </div>
      ) : null}

      {!trashMode ? (
        <div
          ref={menuRef}
          className="context-menu folder-context-menu"
          style={{
            left: Math.min(menu.x, Math.max(8, window.innerWidth - 224)),
            top: Math.min(menu.y, Math.max(8, window.innerHeight - 320)),
          }}
          onContextMenu={(event) => event.preventDefault()}
        >
          {menuIsRoot ? (
            <button
              type="button"
              className="context-menu-item"
              onClick={() => onCreateFolder("")}
            >
              <FolderPlus size={14} />
              <span>新建文件夹</span>
            </button>
          ) : (
            <>
              <button
                type="button"
                className="context-menu-item"
                onClick={() => onCreateFolder(menuNode?.path ?? "")}
              >
                <FolderPlus size={14} />
                <span>新建子文件夹</span>
              </button>
              <button
                type="button"
                className="context-menu-item"
                onClick={() => onRename(menuNode)}
              >
                <Pencil size={14} />
                <span>重命名</span>
              </button>
            </>
          )}
          <button
            type="button"
            className="context-menu-item"
            disabled={sortDisabled}
            title={sortDisabled ? "没有需要排序的子文件夹" : undefined}
            onClick={onSort}
          >
            <ArrowDownUp size={14} />
            <span>排序子文件夹…</span>
          </button>
          {menuIsRoot ? null : (
            <>
              <button
                type="button"
                className="context-menu-item"
                onClick={() => onMove(menuNode)}
              >
                <FolderInput size={14} />
                <span>移动到…</span>
              </button>
              <button
                type="button"
                className="context-menu-item"
                disabled={selectedImageCount === 0}
                onClick={() => onMoveImages(menuNode)}
              >
                <Folder size={14} />
                <span>将选中图片移入</span>
                {selectedImageCount > 0 ? <em>{selectedImageCount}</em> : null}
              </button>
              <div className="context-menu-separator" />
              <button
                type="button"
                className="context-menu-item context-menu-danger"
                onClick={() => onTrash(menuNode)}
              >
                <Trash2 size={14} />
                <span>移入回收站</span>
              </button>
            </>
          )}
        </div>
      ) : null}
    </>
  );
}
