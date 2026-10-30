import {
  Fragment,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  type RefObject,
} from "react";
import { ChevronDown, ChevronRight, Folder, FolderOpen, Images } from "lucide-react";
import type { FolderNode } from "./folderTreeModel";

/** 两个窗格各自的行元素，供外部跳转时把目标滚进可见区域。 */
export type FolderRowRefs = [
  Map<number, HTMLDivElement>,
  Map<number, HTMLDivElement>,
];

interface FolderTreeRowsProps {
  rows: FolderNode[];
  expanded: Set<number>;
  selected: Set<number>;
  directFolderId: number | null;
  trashMode: boolean;
  pane: number;
  rowRefs: RefObject<FolderRowRefs>;
  onActivatePane: (pane: number) => void;
  onToggleExpanded: (id: number) => void;
  onSelectNode: (node: FolderNode, event: ReactMouseEvent, pane: number) => void;
  onRowPointerDown: (
    node: FolderNode,
    pane: number,
    event: ReactPointerEvent<HTMLDivElement>,
  ) => void;
  onOpenMenu: (node: FolderNode, pane: number, event: ReactMouseEvent) => void;
  onSelectDirect: (folderId: number, pane: number) => void;
}

/** 单个窗格里的文件夹行与“直属图片”行；展开、选中状态与回调都由父组件提供。 */
export function FolderTreeRows({
  rows,
  expanded,
  selected,
  directFolderId,
  trashMode,
  pane,
  rowRefs,
  onActivatePane,
  onToggleExpanded,
  onSelectNode,
  onRowPointerDown,
  onOpenMenu,
  onSelectDirect,
}: FolderTreeRowsProps) {
  return (
    <>
      {rows.map((node) => {
        const hasChildren = node.children.length > 0;
        const isExpanded = expanded.has(node.id);
        const isSelected = selected.has(node.id);
        const showDirect = hasChildren && isExpanded && node.directImageCount > 0;
        return (
          <Fragment key={node.id}>
            <div
              ref={(element) => {
                if (element) rowRefs.current[pane].set(node.id, element);
                else rowRefs.current[pane].delete(node.id);
              }}
              className={`folder-tree-row${isSelected ? " is-selected" : ""}`}
              role="treeitem"
              aria-selected={isSelected}
              aria-expanded={hasChildren ? isExpanded : undefined}
              style={{ paddingLeft: 6 + node.depth * 14 }}
              title={
                trashMode
                  ? `${node.path} · 回收站图片 ${node.totalImageCount} 张`
                  : `${node.path} · 直接 ${node.directImageCount} 张 · 含子文件夹 ${node.totalImageCount} 张`
              }
              {...(trashMode ? {} : { "data-drop-folder": node.id })}
              onPointerDown={
                trashMode ? undefined : (event) => onRowPointerDown(node, pane, event)
              }
              onClick={(event) => onSelectNode(node, event, pane)}
              onContextMenu={(event) => onOpenMenu(node, pane, event)}
            >
              <button
                type="button"
                className="folder-tree-toggle"
                aria-label={isExpanded ? "折叠" : "展开"}
                disabled={!hasChildren}
                onClick={(event) => {
                  event.stopPropagation();
                  onActivatePane(pane);
                  if (hasChildren) onToggleExpanded(node.id);
                }}
              >
                {hasChildren ? (
                  isExpanded ? (
                    <ChevronDown size={13} />
                  ) : (
                    <ChevronRight size={13} />
                  )
                ) : (
                  <span className="folder-tree-spacer" />
                )}
              </button>
              {isExpanded ? <FolderOpen size={14} /> : <Folder size={14} />}
              <span className="folder-tree-name">{node.name}</span>
              <em>{node.totalImageCount.toLocaleString()}</em>
            </div>
            {showDirect ? (
              <div
                className={`folder-tree-row folder-tree-direct${
                  directFolderId === node.id ? " is-selected" : ""
                }`}
                role="treeitem"
                aria-selected={directFolderId === node.id}
                style={{ paddingLeft: 6 + (node.depth + 1) * 14 }}
                title={`只显示直接放在「${node.path}」的 ${node.directImageCount.toLocaleString()} 张图片`}
                {...(trashMode ? {} : { "data-drop-direct": node.id })}
                onClick={(event) => {
                  event.stopPropagation();
                  onSelectDirect(node.id, pane);
                }}
              >
                <span className="folder-tree-toggle">
                  <span className="folder-tree-spacer" />
                </span>
                <Images size={13} />
                <span className="folder-tree-name">直属图片</span>
                <em>{node.directImageCount.toLocaleString()}</em>
              </div>
            ) : null}
          </Fragment>
        );
      })}
    </>
  );
}
