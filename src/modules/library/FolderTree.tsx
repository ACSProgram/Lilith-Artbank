import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";
import { FolderOpen, Images, Plus, Rows2, Trash2 } from "lucide-react";
import { IconButton } from "../../shared/components/Button";
import { FolderPickerDialog } from "../../shared/components/FolderPickerDialog";
import { useNotices } from "../../shared/notices";
import { errorMessage } from "../../shared/errors";
import {
  confirmDeleteFolderPermanently,
  confirmRestoreFolder,
  confirmTrashFolder,
} from "../../shared/confirm";
import { FOLDER_DRAG_HOVER_EVENT } from "../../shared/useDragSort";
import { libraryApi } from "./api";
import { FolderNameDialog, FolderSortDialog } from "./FolderDialogs";
import { FolderContextMenu, type FolderMenu } from "./FolderContextMenu";
import { FolderTreeRows, type FolderRowRefs } from "./FolderTreeRows";
import {
  applyFolderChange,
  buildFolderTree,
  flattenVisible,
  hasNestedSortable,
  isSameOrDescendant,
  needsFolderSort,
  sortFolderOrders,
  type FolderNode,
  type FolderSortKey,
} from "./folderTreeModel";
import { useFolderExpansion } from "./useFolderExpansion";
import {
  MAX_SPLIT_RATIO,
  MIN_SPLIT_RATIO,
  useFolderSplit,
} from "./useFolderSplit";
import type { FolderChange, FolderInfo } from "./types";

interface FolderTreeProps {
  libraryId: string;
  folders: FolderInfo[];
  selectedFolderIds: number[];
  directFolderId: number | null;
  selectedImageIds: number[];
  onSelect: (folderIds: number[]) => void;
  onSelectDirect: (folderId: number) => void;
  onFoldersChanged: (folders: FolderInfo[]) => void;
  onDataChanged: (
    delta?: { images?: number; trash?: number },
    reason?: string,
  ) => void;
  /** 把图片移入某个文件夹；带 ids 与目标路径，供图库裁剪选择与刷新。 */
  onImagesMoved?: (imageIds: number[], targetPath: string) => void;
  /** 回收站模式：只读树，只提供恢复与永久删除。 */
  mode?: "normal" | "trash";
  /** 回收站根行“直属图片”的数量与选中状态。 */
  trashDirectCount?: number;
  trashTotalCount?: number;
  trashDirectSelected?: boolean;
  onSelectTrashDirect?: () => void;
  onBeginFolderDrag: (
    folderIds: number[],
    label: string,
    event: ReactPointerEvent<HTMLDivElement>,
  ) => void;
  /** 上下双窗格：共享展开与选中，各自滚动。 */
  split: boolean;
  /** 上窗格占树高度的比例。 */
  splitRatio: number;
  onToggleSplit: () => void;
  onSplitRatioChange: (ratio: number) => void;
}

interface SortTarget {
  id: number;
  path: string;
}

export function FolderTree({
  libraryId,
  folders,
  selectedFolderIds,
  directFolderId,
  selectedImageIds,
  onSelect,
  onSelectDirect,
  onFoldersChanged,
  onDataChanged,
  onImagesMoved,
  onBeginFolderDrag,
  mode = "normal",
  trashDirectCount = 0,
  trashTotalCount = 0,
  trashDirectSelected = false,
  onSelectTrashDirect,
  split,
  splitRatio,
  onToggleSplit,
  onSplitRatioChange,
}: FolderTreeProps) {
  const { notify, confirm } = useNotices();
  const trashMode = mode === "trash";
  const tree = useMemo(() => buildFolderTree(folders), [folders]);
  const { expanded, toggleExpanded, expandFolder } = useFolderExpansion({
    libraryId,
    tree,
    trashMode,
    selectedFolderIds,
  });
  const [anchorId, setAnchorId] = useState<number | null>(null);
  const [menu, setMenu] = useState<FolderMenu | null>(null);
  const [createParent, setCreateParent] = useState<string | null>(null);
  const [renameTarget, setRenameTarget] = useState<FolderNode | null>(null);
  const [moveTarget, setMoveTarget] = useState<FolderNode | null>(null);
  const [sortTarget, setSortTarget] = useState<SortTarget | null>(null);
  const [sortBusy, setSortBusy] = useState(false);
  const [sortError, setSortError] = useState<string | null>(null);
  const [dialogBusy, setDialogBusy] = useState(false);
  const [dialogError, setDialogError] = useState<string | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const paneRefs = useRef<Array<HTMLDivElement | null>>([null, null]);
  const rowRefs = useRef<FolderRowRefs>([new Map(), new Map()]);
  /** 最近一次交互的窗格；外部跳转只滚动它，另一窗格保持原位。 */
  const activePaneRef = useRef(0);
  const lastScrollTop = useRef(0);
  const {
    ratio,
    dividerDragging,
    splitRef,
    onDividerPointerDown,
    onDividerPointerMove,
    finishDividerDrag,
    onDividerKeyDown,
  } = useFolderSplit(splitRatio, onSplitRatioChange);

  const rootFolder = folders.find((folder) => folder.path === "");
  const selected = useMemo(() => new Set(selectedFolderIds), [selectedFolderIds]);
  const rows = useMemo(() => flattenVisible(tree.nodes, expanded), [tree, expanded]);
  const rootSelected = trashMode
    ? selectedFolderIds.length === 0 && !trashDirectSelected
    : selectedFolderIds.length === 0;

  // 切换单/双视图时把原来的滚动位置交给可见窗格，避免一开双视图就跳回顶部。
  useLayoutEffect(() => {
    const pane = paneRefs.current[0];
    if (pane) pane.scrollTop = lastScrollTop.current;
  }, [split]);

  // 外部跳转（详情“转到所在文件夹”）后把目标行滚进可见区域；只动最近交互的窗格。
  const scrollTargetId = selectedFolderIds.length === 1 ? selectedFolderIds[0] : null;
  useEffect(() => {
    if (scrollTargetId == null) return;
    for (const pane of split ? [activePaneRef.current] : [0]) {
      rowRefs.current[pane].get(scrollTargetId)?.scrollIntoView({ block: "nearest" });
    }
  }, [scrollTargetId, rows, split]);

  useEffect(() => {
    const onHover = (event: Event) => {
      const folderId = (event as CustomEvent<{ folderId: number | null }>).detail?.folderId;
      if (folderId == null) return;
      expandFolder(folderId);
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

  const selectNode = (node: FolderNode, event: ReactMouseEvent, pane: number) => {
    activePaneRef.current = pane;
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

  const selectRoot = (pane: number) => {
    activePaneRef.current = pane;
    setMenu(null);
    setAnchorId(null);
    onSelect([]);
  };

  const onRowPointerDown = (
    node: FolderNode,
    pane: number,
    event: ReactPointerEvent<HTMLDivElement>,
  ) => {
    activePaneRef.current = pane;
    const ids =
      selected.size > 1 && selected.has(node.id)
        ? selectedFolderIds.filter((id) => tree.byId.has(id))
        : [node.id];
    onBeginFolderDrag(
      ids,
      ids.length > 1 ? `${ids.length} 个文件夹` : node.name,
      event,
    );
  };

  const onOpenFolderMenu = (
    node: FolderNode,
    pane: number,
    event: ReactMouseEvent,
  ) => {
    event.preventDefault();
    event.stopPropagation();
    activePaneRef.current = pane;
    if (!selected.has(node.id)) {
      onSelect([node.id]);
      setAnchorId(node.id);
    }
    setMenu({ x: event.clientX, y: event.clientY, node });
  };

  const onSelectDirectRow = (folderId: number, pane: number) => {
    activePaneRef.current = pane;
    setMenu(null);
    onSelectDirect(folderId);
  };

  const refreshAfterOperation = useCallback(
    (
      change: FolderChange,
      delta?: { images?: number; trash?: number },
      reason?: string,
    ) => {
      onFoldersChanged(applyFolderChange(folders, change));
      onDataChanged(delta, reason);
    },
    [folders, onDataChanged, onFoldersChanged],
  );

  const createFolder = async (name: string) => {
    if (createParent == null) return;
    const path = createParent ? `${createParent}/${name}` : name;
    setDialogBusy(true);
    setDialogError(null);
    try {
      const change = await libraryApi.createFolder(path);
      refreshAfterOperation(change);
      setCreateParent(null);
      const created = change.folders.find((folder) => folder.path === path);
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
      const change = await libraryApi.renameFolder(renameTarget.id, name);
      refreshAfterOperation(change);
      setRenameTarget(null);
    } catch (cause) {
      setDialogError(errorMessage(cause));
    } finally {
      setDialogBusy(false);
    }
  };

  const deleteFolder = async (node: FolderNode) => {
    const accepted = await confirmTrashFolder(confirm, node.path, node.totalImageCount);
    if (!accepted) return;
    try {
      const change = await libraryApi.deleteFolder(node.id);
      refreshAfterOperation(
        change,
        { images: -change.affectedImages, trash: change.affectedImages },
        "folderTrash",
      );
      notify(
        change.affectedImages > 0
          ? `文件夹已移入回收站，共 ${change.affectedImages.toLocaleString()} 张图片`
          : "文件夹已移入回收站",
        "success",
      );
      if (
        selectedFolderIds.some((id) => {
          const path = tree.byId.get(id)?.path;
          return path != null && isSameOrDescendant(path, node.path);
        })
      ) {
        onSelect([]);
      }
    } catch (cause) {
      notify("移入回收站失败", "error", errorMessage(cause));
    }
  };

  const restoreFolderNode = async (node: FolderNode) => {
    const accepted = await confirmRestoreFolder(confirm, node.path);
    if (!accepted) return;
    try {
      const change = await libraryApi.restoreFolder(node.id);
      onDataChanged(
        { images: change.affectedImages, trash: -change.affectedImages },
        "folderRestore",
      );
      notify(`已恢复文件夹「${node.name}」`, "success");
      if (selectedFolderIds.some((id) => id === node.id)) onSelect([]);
    } catch (cause) {
      notify("恢复文件夹失败", "error", errorMessage(cause));
    }
  };

  const deleteFolderPermanently = async (node: FolderNode) => {
    const accepted = await confirmDeleteFolderPermanently(
      confirm,
      node.path,
      node.totalImageCount,
    );
    if (!accepted) return;
    try {
      const change = await libraryApi.deleteFolderPermanently(node.id);
      onDataChanged(
        { trash: -change.affectedImages },
        "folderPurge",
      );
      notify(`已从回收站移除文件夹「${node.name}」`, "success");
      if (selectedFolderIds.some((id) => id === node.id)) onSelect([]);
    } catch (cause) {
      notify("永久删除文件夹失败", "error", errorMessage(cause));
    }
  };

  const moveFolderTo = async (folderId: number, parentId: number) => {
    try {
      const change = await libraryApi.moveFolder(folderId, parentId);
      refreshAfterOperation(change);
      setMoveTarget(null);
    } catch (cause) {
      notify("移动文件夹失败", "error", errorMessage(cause));
    }
  };

  const moveImagesTo = async (imageIds: number[], node: FolderNode | null) => {
    if (imageIds.length === 0) return;
    const targetPath = node?.path ?? "";
    try {
      const report = await libraryApi.moveImagesToFolder(imageIds, targetPath);
      // 移动图片只改计数，不增删文件夹；直接整树替换一次，比按 id 合并更省事。
      onFoldersChanged(await libraryApi.getFolders());
      if (onImagesMoved) {
        onImagesMoved(imageIds, targetPath);
      } else {
        onDataChanged();
      }
      notify(
        `已移动 ${report.moved.toLocaleString()} 张，跳过 ${report.skipped.toLocaleString()} 张`,
        report.failures.length > 0 ? "error" : "success",
        report.failures.length > 0 ? report.failures[0].reason : undefined,
      );
    } catch (cause) {
      notify("移动图片失败", "error", errorMessage(cause));
    }
  };

  const sortChildren = async (
    key: FolderSortKey,
    descending: boolean,
    recursive: boolean,
  ) => {
    if (!sortTarget) return;
    const orders = sortFolderOrders(tree, sortTarget.id, key, descending, recursive);
    if (orders.length === 0) {
      setSortTarget(null);
      notify("没有需要排序的子文件夹", "info");
      return;
    }
    setSortBusy(true);
    setSortError(null);
    try {
      const change = await libraryApi.applyFolderOrder(
        sortTarget.id,
        key,
        descending,
        recursive,
        orders,
      );
      onFoldersChanged(applyFolderChange(folders, change));
      setSortTarget(null);
      const count = orders.reduce((sum, order) => sum + order.folderIds.length, 0);
      notify(`已调整 ${count.toLocaleString()} 个文件夹的顺序`, "success");
    } catch (cause) {
      setSortError(errorMessage(cause));
    } finally {
      setSortBusy(false);
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
  const menuIsRoot = menu != null && menu.node == null;
  const canSort = menuNode ? needsFolderSort(tree, menuNode.id) : false;
  const canSortRoot = tree.rootId != null && needsFolderSort(tree, tree.rootId);
  const sortDisabled = menuIsRoot ? !canSortRoot : !canSort;

  const openCreateDialog = (parentPath: string) => {
    setMenu(null);
    setDialogError(null);
    setCreateParent(parentPath);
  };

  const openRenameDialog = (node: FolderNode | null) => {
    setMenu(null);
    setDialogError(null);
    setRenameTarget(node);
  };

  const openSortDialog = () => {
    setMenu(null);
    setSortError(null);
    if (menuIsRoot) {
      if (tree.rootId != null) setSortTarget({ id: tree.rootId, path: "" });
    } else if (menuNode) {
      setSortTarget({ id: menuNode.id, path: menuNode.path });
    }
  };

  const openMoveDialog = (node: FolderNode | null) => {
    setMenu(null);
    setMoveTarget(node);
  };

  const menuRestore = (node: FolderNode | null) => {
    setMenu(null);
    if (node) void restoreFolderNode(node);
  };

  const menuDeletePermanently = (node: FolderNode | null) => {
    setMenu(null);
    if (node) void deleteFolderPermanently(node);
  };

  const menuMoveImages = (node: FolderNode | null) => {
    setMenu(null);
    if (node) void moveImagesTo(selectedImageIds, node);
  };

  const menuTrash = (node: FolderNode | null) => {
    setMenu(null);
    if (node) void deleteFolder(node);
  };

  /** 两个窗格渲染同一份行；展开、选中与菜单共享，滚动位置各自独立。 */
  const renderTree = (pane: number) => (
    <div
      ref={(element) => {
        paneRefs.current[pane] = element;
      }}
      className="folder-tree-scroll"
      role="tree"
      aria-label={pane === 0 ? (split ? "文件夹树（上）" : "文件夹树") : "文件夹树（下）"}
      data-drag-scroll="true"
      onScroll={(event) => {
        lastScrollTop.current = event.currentTarget.scrollTop;
      }}
    >
      <div
        className={`folder-tree-row folder-tree-root${rootSelected ? " is-selected" : ""}`}
        role="treeitem"
        aria-selected={rootSelected}
        {...(trashMode ? {} : { "data-drop-root": "true" })}
        onClick={() => {
          activePaneRef.current = pane;
          setMenu(null);
          if (trashMode) {
            setAnchorId(null);
            onSelect([]);
          } else {
            selectRoot(pane);
          }
        }}
        onContextMenu={
          trashMode
            ? undefined
            : (event) => {
                event.preventDefault();
                event.stopPropagation();
                activePaneRef.current = pane;
                setMenu({ x: event.clientX, y: event.clientY, node: null });
              }
        }
      >
        <span className="folder-tree-toggle">
          <span className="folder-tree-spacer" />
        </span>
        {trashMode ? <Trash2 size={15} /> : <FolderOpen size={15} />}
        <span className="folder-tree-name">{trashMode ? "回收站" : "全部图片"}</span>
        {trashMode ? (
          <em>{trashTotalCount.toLocaleString()}</em>
        ) : rootFolder ? (
          <em>{rootFolder.totalImageCount.toLocaleString()}</em>
        ) : null}
      </div>

      {trashMode ? (
        <div
          className={`folder-tree-row folder-tree-direct${trashDirectSelected ? " is-selected" : ""}`}
          role="treeitem"
          aria-selected={trashDirectSelected}
          style={{ paddingLeft: 6 + 14 }}
          title="只显示原文件夹仍在图库中的回收站图片"
          onClick={(event) => {
            event.stopPropagation();
            activePaneRef.current = pane;
            setMenu(null);
            onSelectTrashDirect?.();
          }}
        >
          <span className="folder-tree-toggle">
            <span className="folder-tree-spacer" />
          </span>
          <Images size={13} />
          <span className="folder-tree-name">直属图片</span>
          <em>{trashDirectCount.toLocaleString()}</em>
        </div>
      ) : null}

      <FolderTreeRows
        rows={rows}
        expanded={expanded}
        selected={selected}
        directFolderId={directFolderId}
        trashMode={trashMode}
        pane={pane}
        rowRefs={rowRefs}
        onActivatePane={(target) => {
          activePaneRef.current = target;
        }}
        onToggleExpanded={toggleExpanded}
        onSelectNode={selectNode}
        onRowPointerDown={onRowPointerDown}
        onOpenMenu={onOpenFolderMenu}
        onSelectDirect={onSelectDirectRow}
      />
    </div>
  );

  return (
    <div className="folder-tree">
      <div className="folder-tree-heading">
        <span>{trashMode ? "回收站" : "文件夹"}</span>
        {trashMode ? null : (
          <div className="folder-tree-heading-actions">
            <IconButton
              label={split ? "合并为单视图" : "分为上下两个视图"}
              className={split ? "is-active" : undefined}
              aria-pressed={split}
              onClick={onToggleSplit}
            >
              <Rows2 size={14} />
            </IconButton>
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
        )}
      </div>

      {!trashMode && split ? (
        <div className="folder-tree-split" ref={splitRef}>
          <div className="folder-tree-pane" style={{ flex: `${ratio} 1 0` }}>
            {renderTree(0)}
          </div>
          <div
            className={`folder-tree-divider${dividerDragging ? " is-dragging" : ""}`}
            role="separator"
            aria-orientation="horizontal"
            aria-label="调整上下两个视图的高度"
            aria-valuenow={Math.round(ratio * 100)}
            aria-valuemin={MIN_SPLIT_RATIO * 100}
            aria-valuemax={MAX_SPLIT_RATIO * 100}
            tabIndex={0}
            onPointerDown={onDividerPointerDown}
            onPointerMove={onDividerPointerMove}
            onPointerUp={finishDividerDrag}
            onPointerCancel={finishDividerDrag}
            onKeyDown={onDividerKeyDown}
          />
          <div className="folder-tree-pane" style={{ flex: `${1 - ratio} 1 0` }}>
            {renderTree(1)}
          </div>
        </div>
      ) : (
        renderTree(0)
      )}

      {menu ? (
        <FolderContextMenu
          menu={menu}
          menuRef={menuRef}
          trashMode={trashMode}
          menuIsRoot={menuIsRoot}
          sortDisabled={sortDisabled}
          selectedImageCount={selectedImageIds.length}
          onRestore={menuRestore}
          onDeletePermanently={menuDeletePermanently}
          onCreateFolder={openCreateDialog}
          onRename={openRenameDialog}
          onSort={openSortDialog}
          onMove={openMoveDialog}
          onMoveImages={menuMoveImages}
          onTrash={menuTrash}
        />
      ) : null}

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
      <FolderSortDialog
        open={sortTarget != null}
        path={sortTarget?.path ?? ""}
        canRecurse={sortTarget != null && hasNestedSortable(tree, sortTarget.id)}
        busy={sortBusy}
        error={sortError}
        onClose={() => {
          if (!sortBusy) setSortTarget(null);
        }}
        onSubmit={(key, descending, recursive) =>
          void sortChildren(key, descending, recursive)
        }
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
