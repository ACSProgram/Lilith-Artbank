import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";
import {
  ArrowDownUp,
  ChevronDown,
  ChevronRight,
  Folder,
  FolderInput,
  FolderOpen,
  FolderPlus,
  Pencil,
  Plus,
  Trash2,
} from "lucide-react";
import { IconButton } from "../../shared/components/Button";
import { FolderPickerDialog } from "../../shared/components/FolderPickerDialog";
import { MenuSection } from "../../shared/components/MenuSection";
import { Popover } from "../../shared/components/Popover";
import { useNotices } from "../../app/noticeContext";
import { errorMessage } from "../../shared/errors";
import { FOLDER_DRAG_HOVER_EVENT } from "../../shared/useDragSort";
import { libraryApi } from "./api";
import { FolderNameDialog } from "./FolderDialogs";
import {
  DEFAULT_FOLDER_SORT,
  FOLDER_SORT_LABELS,
  ancestorPaths,
  buildFolderTree,
  flattenVisible,
  type FolderNode,
  type FolderSortMode,
  type FolderSortSpec,
} from "./folderTreeModel";
import type { FolderInfo } from "./types";

interface FolderTreeProps {
  folders: FolderInfo[];
  selectedFolderIds: number[];
  selectedImageIds: number[];
  onSelect: (folderIds: number[]) => void;
  onFoldersChanged: (folders: FolderInfo[]) => void;
  onDataChanged: () => void;
  onBeginFolderDrag: (
    folderId: number,
    label: string,
    event: ReactPointerEvent<HTMLDivElement>,
  ) => void;
}

interface FolderMenu {
  x: number;
  y: number;
  node: FolderNode;
}

function initialFolderSort(): FolderSortSpec {
  try {
    const raw = localStorage.getItem("lilith.folderSort");
    if (!raw) return DEFAULT_FOLDER_SORT;
    const parsed = JSON.parse(raw) as Partial<FolderSortSpec>;
    const modes: FolderSortMode[] = ["manual", "name", "total", "direct"];
    if (!parsed.mode || !modes.includes(parsed.mode)) return DEFAULT_FOLDER_SORT;
    return { mode: parsed.mode, descending: Boolean(parsed.descending) };
  } catch {
    return DEFAULT_FOLDER_SORT;
  }
}

export function FolderTree({
  folders,
  selectedFolderIds,
  selectedImageIds,
  onSelect,
  onFoldersChanged,
  onDataChanged,
  onBeginFolderDrag,
}: FolderTreeProps) {
  const { notify, confirm } = useNotices();
  const [folderSort, setFolderSort] = useState<FolderSortSpec>(initialFolderSort);
  const [sortOpen, setSortOpen] = useState(false);
  const sortAnchorRef = useRef<HTMLSpanElement>(null);
  const tree = useMemo(() => buildFolderTree(folders, folderSort), [folders, folderSort]);
  const [expanded, setExpanded] = useState<Set<number>>(() => new Set());
  const [anchorId, setAnchorId] = useState<number | null>(null);
  const [menu, setMenu] = useState<FolderMenu | null>(null);
  const [createParent, setCreateParent] = useState<string | null>(null);
  const [renameTarget, setRenameTarget] = useState<FolderNode | null>(null);
  const [moveTarget, setMoveTarget] = useState<FolderNode | null>(null);
  const [dialogBusy, setDialogBusy] = useState(false);
  const [dialogError, setDialogError] = useState<string | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const initialized = useRef(false);

  const rootFolder = folders.find((folder) => folder.path === "");
  const selected = useMemo(() => new Set(selectedFolderIds), [selectedFolderIds]);
  const rows = useMemo(() => flattenVisible(tree.nodes, expanded), [tree, expanded]);

  useEffect(() => {
    if (initialized.current || tree.nodes.length === 0) return;
    initialized.current = true;
    setExpanded(new Set(tree.nodes.map((node) => node.id)));
  }, [tree]);

  useEffect(() => {
    if (selectedFolderIds.length === 0) return;
    setExpanded((current) => {
      const next = new Set(current);
      let changed = false;
      for (const id of selectedFolderIds) {
        const node = tree.byId.get(id);
        if (!node) continue;
        for (const path of ancestorPaths(node.path)) {
          const ancestor = tree.byPath.get(path);
          if (ancestor && !next.has(ancestor.id)) {
            next.add(ancestor.id);
            changed = true;
          }
        }
      }
      return changed ? next : current;
    });
  }, [selectedFolderIds, tree]);

  useEffect(() => {
    try {
      localStorage.setItem("lilith.folderSort", JSON.stringify(folderSort));
    } catch {
      // 忽略浏览器限制；排序方式仍在本会话内生效。
    }
  }, [folderSort]);

  useEffect(() => {
    const onHover = (event: Event) => {
      const folderId = (event as CustomEvent<{ folderId: number | null }>).detail?.folderId;
      if (folderId == null) return;
      setExpanded((current) => {
        if (current.has(folderId)) return current;
        const next = new Set(current);
        next.add(folderId);
        return next;
      });
    };
    window.addEventListener(FOLDER_DRAG_HOVER_EVENT, onHover);
    return () => window.removeEventListener(FOLDER_DRAG_HOVER_EVENT, onHover);
  }, []);

  useEffect(() => {
    if (!menu) return;
    const onPointerDown = (event: PointerEvent) => {
      if (!menuRef.current?.contains(event.target as Node)) setMenu(null);
    };
    const onScroll = () => setMenu(null);
    document.addEventListener("pointerdown", onPointerDown);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [menu]);

  const toggleExpanded = (id: number) => {
    setExpanded((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const selectNode = (node: FolderNode, event: ReactMouseEvent) => {
    setMenu(null);
    if (event.shiftKey && anchorId != null) {
      const ids = rows.map((row) => row.id);
      const start = ids.indexOf(anchorId);
      const end = ids.indexOf(node.id);
      if (start >= 0 && end >= 0) {
        const from = Math.min(start, end);
        const to = Math.max(start, end);
        onSelect(ids.slice(from, to + 1));
        return;
      }
    }
    if (event.ctrlKey || event.metaKey) {
      const next = new Set(selectedFolderIds);
      if (next.has(node.id)) next.delete(node.id);
      else next.add(node.id);
      onSelect([...next]);
      setAnchorId(node.id);
      return;
    }
    onSelect([node.id]);
    setAnchorId(node.id);
  };

  const selectRoot = () => {
    setMenu(null);
    setAnchorId(null);
    onSelect([]);
  };

  const refreshAfterOperation = useCallback(
    (next: FolderInfo[]) => {
      onFoldersChanged(next);
      onDataChanged();
    },
    [onDataChanged, onFoldersChanged],
  );

  const createFolder = async (name: string) => {
    if (createParent == null) return;
    const path = createParent ? `${createParent}/${name}` : name;
    setDialogBusy(true);
    setDialogError(null);
    try {
      const next = await libraryApi.createFolder(path);
      refreshAfterOperation(next);
      setCreateParent(null);
      const created = next.find((folder) => folder.path === path);
      if (created) {
        onSelect([created.id]);
        setAnchorId(created.id);
      }
    } catch (cause) {
      setDialogError(errorMessage(cause));
    } finally {
      setDialogBusy(false);
    }
  };

  const renameFolder = async (name: string) => {
    if (!renameTarget) return;
    setDialogBusy(true);
    setDialogError(null);
    try {
      const next = await libraryApi.renameFolder(renameTarget.id, name);
      refreshAfterOperation(next);
      setRenameTarget(null);
    } catch (cause) {
      setDialogError(errorMessage(cause));
    } finally {
      setDialogBusy(false);
    }
  };

  const deleteFolder = async (node: FolderNode) => {
    const accepted = await confirm({
      title: `删除空文件夹「${node.name}」？`,
      message: "只删除磁盘上的空目录与文件夹记录，不会删除图片。",
      confirmLabel: "删除",
      danger: true,
    });
    if (!accepted) return;
    try {
      const next = await libraryApi.deleteFolder(node.id);
      refreshAfterOperation(next);
      if (selected.has(node.id)) {
        onSelect(selectedFolderIds.filter((id) => id !== node.id));
      }
    } catch (cause) {
      notify("删除文件夹失败", "error", errorMessage(cause));
    }
  };

  const moveFolderTo = async (folderId: number, parentId: number) => {
    try {
      const next = await libraryApi.moveFolder(folderId, parentId);
      refreshAfterOperation(next);
      setMoveTarget(null);
    } catch (cause) {
      notify("移动文件夹失败", "error", errorMessage(cause));
    }
  };

  const moveImagesTo = async (imageIds: number[], node: FolderNode | null) => {
    if (imageIds.length === 0) return;
    try {
      const report = await libraryApi.moveImagesToFolder(imageIds, node?.path ?? "");
      const next = await libraryApi.getFolders();
      refreshAfterOperation(next);
      notify(
        `已移动 ${report.moved.toLocaleString()} 张，跳过 ${report.skipped.toLocaleString()} 张`,
        report.failures.length > 0 ? "error" : "success",
        report.failures.length > 0 ? report.failures[0].reason : undefined,
      );
    } catch (cause) {
      notify("移动图片失败", "error", errorMessage(cause));
    }
  };

  const disabledMoveIds = useMemo(() => {
    if (!moveTarget) return new Set<number>();
    const ids = new Set<number>();
    const prefix = `${moveTarget.path}/`;
    for (const folder of folders) {
      if (folder.id === moveTarget.id || folder.path.startsWith(prefix)) {
        ids.add(folder.id);
      }
    }
    return ids;
  }, [folders, moveTarget]);

  const menuNode = menu?.node ?? null;

  return (
    <div className="folder-tree">
      <div className="folder-tree-heading">
        <span>文件夹</span>
        <div className="folder-tree-heading-actions">
          <span ref={sortAnchorRef} className="folder-tree-sort-anchor">
            <IconButton
              label={`文件夹排序：${FOLDER_SORT_LABELS[folderSort.mode]}${folderSort.descending ? "降序" : "升序"}`}
              className={folderSort.mode === "manual" ? "" : "is-active"}
              onClick={() => setSortOpen((open) => !open)}
            >
              <ArrowDownUp size={14} />
            </IconButton>
          </span>
          <IconButton
            label="新建文件夹"
            onClick={() => {
              setDialogError(null);
              setCreateParent("");
            }}
          >
            <Plus size={14} />
          </IconButton>
        </div>
      </div>

      <div
        className="folder-tree-scroll"
        role="tree"
        aria-label="文件夹树"
        data-drag-scroll="true"
      >
        <div
          className={`folder-tree-row folder-tree-root${
            selectedFolderIds.length === 0 ? " is-selected" : ""
          }`}
          role="treeitem"
          aria-selected={selectedFolderIds.length === 0}
          data-drop-root="true"
          onClick={selectRoot}
        >
          <span className="folder-tree-toggle">
            <span className="folder-tree-spacer" />
          </span>
          <FolderOpen size={15} />
          <span className="folder-tree-name">全部图片</span>
          {rootFolder ? <em>{rootFolder.totalImageCount.toLocaleString()}</em> : null}
        </div>

        {rows.map((node) => {
          const hasChildren = node.children.length > 0;
          const isExpanded = expanded.has(node.id);
          const isSelected = selected.has(node.id);
          return (
            <div
              key={node.id}
              className={`folder-tree-row${isSelected ? " is-selected" : ""}`}
              role="treeitem"
              aria-selected={isSelected}
              aria-expanded={hasChildren ? isExpanded : undefined}
              style={{ paddingLeft: 6 + node.depth * 14 }}
              title={`${node.path} · 直接 ${node.directImageCount} 张 · 含子文件夹 ${node.totalImageCount} 张`}
              data-drop-folder={node.id}
              onPointerDown={(event) => {
                if (folderSort.mode === "manual") onBeginFolderDrag(node.id, node.name, event);
              }}
              onClick={(event) => selectNode(node, event)}
              onContextMenu={(event) => {
                event.preventDefault();
                event.stopPropagation();
                if (!selected.has(node.id)) {
                  onSelect([node.id]);
                  setAnchorId(node.id);
                }
                setMenu({ x: event.clientX, y: event.clientY, node });
              }}
            >
              <button
                type="button"
                className="folder-tree-toggle"
                aria-label={isExpanded ? "折叠" : "展开"}
                disabled={!hasChildren}
                onClick={(event) => {
                  event.stopPropagation();
                  if (hasChildren) toggleExpanded(node.id);
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
          );
        })}
      </div>

      {menu ? (
        <div
          ref={menuRef}
          className="context-menu folder-context-menu"
          style={{
            left: Math.min(menu.x, Math.max(8, window.innerWidth - 224)),
            top: Math.min(menu.y, Math.max(8, window.innerHeight - 240)),
          }}
          onContextMenu={(event) => event.preventDefault()}
        >
          <button
            type="button"
            className="context-menu-item"
            onClick={() => {
              setMenu(null);
              setDialogError(null);
              setCreateParent(menuNode?.path ?? "");
            }}
          >
            <FolderPlus size={14} />
            <span>新建子文件夹</span>
          </button>
          <button
            type="button"
            className="context-menu-item"
            onClick={() => {
              setMenu(null);
              setDialogError(null);
              setRenameTarget(menuNode);
            }}
          >
            <Pencil size={14} />
            <span>重命名</span>
          </button>
          <button
            type="button"
            className="context-menu-item"
            onClick={() => {
              setMenu(null);
              setMoveTarget(menuNode);
            }}
          >
            <FolderInput size={14} />
            <span>移动到…</span>
          </button>
          <button
            type="button"
            className="context-menu-item"
            disabled={selectedImageIds.length === 0}
            onClick={() => {
              const target = menuNode;
              setMenu(null);
              if (target) void moveImagesTo(selectedImageIds, target);
            }}
          >
            <Folder size={14} />
            <span>将选中图片移入</span>
            {selectedImageIds.length > 0 ? <em>{selectedImageIds.length}</em> : null}
          </button>
          <div className="context-menu-separator" />
          <button
            type="button"
            className="context-menu-item context-menu-danger"
            disabled={
              !menuNode ||
              menuNode.children.length > 0 ||
              menuNode.directImageCount > 0
            }
            title={
              menuNode && (menuNode.children.length > 0 || menuNode.directImageCount > 0)
                ? "只能删除没有子文件夹且没有图片的空文件夹"
                : undefined
            }
            onClick={() => {
              const target = menuNode;
              setMenu(null);
              if (target) void deleteFolder(target);
            }}
          >
            <Trash2 size={14} />
            <span>删除空文件夹</span>
          </button>
        </div>
      ) : null}

      <Popover
        open={sortOpen}
        anchorRef={sortAnchorRef}
        placement="bottom-start"
        onClose={() => setSortOpen(false)}
        minWidth={220}
      >
        <MenuSection title="排序方式">
          {(Object.keys(FOLDER_SORT_LABELS) as FolderSortMode[]).map((mode) => (
            <button
              key={mode}
              type="button"
              className={`context-menu-item${folderSort.mode === mode ? " is-active" : ""}`}
              onClick={() => setFolderSort((current) => ({ ...current, mode }))}
            >
              <span>{FOLDER_SORT_LABELS[mode]}</span>
            </button>
          ))}
        </MenuSection>
        <MenuSection title="方向">
          <button
            type="button"
            className={`context-menu-item${folderSort.descending ? "" : " is-active"}`}
            onClick={() => setFolderSort((current) => ({ ...current, descending: false }))}
          >
            <span>升序</span>
          </button>
          <button
            type="button"
            className={`context-menu-item${folderSort.descending ? " is-active" : ""}`}
            onClick={() => setFolderSort((current) => ({ ...current, descending: true }))}
          >
            <span>降序</span>
          </button>
        </MenuSection>
        {folderSort.mode === "manual" ? null : (
          <div className="folder-sort-note">切换到“手动”后才能拖动文件夹调整顺序。</div>
        )}
      </Popover>

      <FolderNameDialog
        open={createParent != null}
        title="新建文件夹"
        subtitle={createParent ? `父文件夹：${createParent}` : "将在图库根目录创建"}
        label="文件夹名称"
        confirmLabel="创建"
        busy={dialogBusy}
        error={dialogError}
        onClose={() => {
          if (!dialogBusy) setCreateParent(null);
        }}
        onSubmit={(name) => void createFolder(name)}
      />
      <FolderNameDialog
        open={renameTarget != null}
        title="重命名文件夹"
        subtitle={renameTarget?.path}
        label="新名称"
        initialValue={renameTarget?.name ?? ""}
        confirmLabel="重命名"
        busy={dialogBusy}
        error={dialogError}
        onClose={() => {
          if (!dialogBusy) setRenameTarget(null);
        }}
        onSubmit={(name) => void renameFolder(name)}
      />
      <FolderPickerDialog
        open={moveTarget != null}
        title="移动文件夹"
        subtitle={moveTarget ? `移动「${moveTarget.path}」到…` : undefined}
        folders={folders}
        disabledIds={disabledMoveIds}
        onClose={() => setMoveTarget(null)}
        onSelect={(target) => {
          if (!moveTarget || target.id == null) return;
          void moveFolderTo(moveTarget.id, target.id);
        }}
      />
    </div>
  );
}
