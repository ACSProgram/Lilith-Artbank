import {
  Fragment,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
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
  Images,
  Pencil,
  Plus,
  RotateCcw,
  Rows2,
  Trash2,
} from "lucide-react";
import { IconButton } from "../../shared/components/Button";
import { FolderPickerDialog } from "../../shared/components/FolderPickerDialog";
import { useNotices } from "../../app/noticeContext";
import { errorMessage } from "../../shared/errors";
import {
  confirmDeleteFolderPermanently,
  confirmRestoreFolder,
  confirmTrashFolder,
} from "../../shared/confirm";
import { FOLDER_DRAG_HOVER_EVENT } from "../../shared/useDragSort";
import { libraryApi } from "./api";
import { FolderNameDialog, FolderSortDialog } from "./FolderDialogs";
import {
  ancestorPaths,
  applyFolderChange,
  buildFolderTree,
  defaultExpandedIds,
  expandedPaths,
  flattenVisible,
  hasNestedSortable,
  isSameOrDescendant,
  needsFolderSort,
  parentFolderPath,
  resolveExpandedPaths,
  sortFolderOrders,
  type FolderNode,
  type FolderSortKey,
} from "./folderTreeModel";
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

const MIN_SPLIT_RATIO = 0.2;
const MAX_SPLIT_RATIO = 0.8;

function clampSplitRatio(value: number): number {
  return Math.min(MAX_SPLIT_RATIO, Math.max(MIN_SPLIT_RATIO, value));
}

interface FolderMenu {
  x: number;
  y: number;
  /** null 表示“全部图片”根行。 */
  node: FolderNode | null;
}

interface SortTarget {
  id: number;
  path: string;
}

const EXPANDED_STORAGE_PREFIX = "lilith.folderExpanded.";

function readExpandedPaths(libraryId: string): string[] | null {
  try {
    const raw = localStorage.getItem(`${EXPANDED_STORAGE_PREFIX}${libraryId}`);
    if (raw == null) return null;
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed) || !parsed.every((item) => typeof item === "string")) {
      return null;
    }
    return parsed;
  } catch {
    return null;
  }
}

function writeExpandedPaths(libraryId: string, paths: string[]) {
  try {
    localStorage.setItem(`${EXPANDED_STORAGE_PREFIX}${libraryId}`, JSON.stringify(paths));
  } catch {
    // 忽略浏览器限制；展开状态仍在本会话内生效。
  }
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
  const [expanded, setExpanded] = useState<Set<number>>(() => new Set());
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
  const splitRef = useRef<HTMLDivElement>(null);
  const paneRefs = useRef<Array<HTMLDivElement | null>>([null, null]);
  /** 两个窗格各自的行元素，供外部跳转时把目标滚进可见区域。 */
  const rowRefs = useRef<[Map<number, HTMLDivElement>, Map<number, HTMLDivElement>]>([
    new Map(),
    new Map(),
  ]);
  /** 最近一次交互的窗格；外部跳转只滚动它，另一窗格保持原位。 */
  const activePaneRef = useRef(0);
  const lastScrollTop = useRef(0);
  const [ratioDraft, setRatioDraft] = useState<number | null>(null);
  const [dividerDragging, setDividerDragging] = useState(false);
  const dragRatioRef = useRef<number | null>(null);
  const hydrated = useRef(false);
  const skipPersist = useRef(true);

  const rootFolder = folders.find((folder) => folder.path === "");
  const selected = useMemo(() => new Set(selectedFolderIds), [selectedFolderIds]);
  const rows = useMemo(() => flattenVisible(tree.nodes, expanded), [tree, expanded]);
  const rootSelected = trashMode
    ? selectedFolderIds.length === 0 && !trashDirectSelected
    : selectedFolderIds.length === 0;

  // 进入回收站时先展开全部顶层节点；正常树的展开状态暂存，切回后原样恢复。
  const lastTrashMode = useRef(trashMode);
  const normalExpandedRef = useRef<Set<number> | null>(null);
  useEffect(() => {
    if (lastTrashMode.current !== trashMode) {
      lastTrashMode.current = trashMode;
      if (trashMode) {
        normalExpandedRef.current = expanded;
        setExpanded(new Set(defaultExpandedIds(tree.nodes)));
      } else {
        setExpanded(normalExpandedRef.current ?? new Set(defaultExpandedIds(tree.nodes)));
        normalExpandedRef.current = null;
      }
      return;
    }
    if (trashMode && expanded.size === 0 && tree.nodes.length > 0) {
      setExpanded(new Set(defaultExpandedIds(tree.nodes)));
    }
  }, [trashMode, tree]);

  // 换库时 Sidebar 用 libraryId 作 key 重新挂载本组件，展开状态无需手动重置。
  useEffect(() => {
    if (trashMode) return;
    if (hydrated.current || tree.nodes.length === 0) return;
    hydrated.current = true;
    skipPersist.current = true;
    const saved = readExpandedPaths(libraryId);
    setExpanded(
      saved ? resolveExpandedPaths(saved, tree.byPath) : defaultExpandedIds(tree.nodes),
    );
  }, [tree, libraryId]);

  useEffect(() => {
    if (trashMode || !hydrated.current) return;
    if (skipPersist.current) {
      skipPersist.current = false;
      return;
    }
    writeExpandedPaths(libraryId, expandedPaths(expanded, tree.byId));
  }, [expanded, tree, libraryId]);

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

  const ratio = ratioDraft ?? splitRatio;

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

  const onDividerPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    dragRatioRef.current = ratio;
    setRatioDraft(ratio);
    setDividerDragging(true);
  };

  const onDividerPointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (dragRatioRef.current == null) return;
    const rect = splitRef.current?.getBoundingClientRect();
    if (!rect || rect.height <= 0) return;
    const next = clampSplitRatio((event.clientY - rect.top) / rect.height);
    dragRatioRef.current = next;
    setRatioDraft(next);
  };

  const finishDividerDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
    const next = dragRatioRef.current;
    if (next == null) return;
    dragRatioRef.current = null;
    setDividerDragging(false);
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    if (next !== splitRatio) onSplitRatioChange(next);
  };

  const onDividerKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const step =
      event.key === "ArrowUp" ? -0.05 : event.key === "ArrowDown" ? 0.05 : 0;
    if (step === 0) return;
    event.preventDefault();
    const next = clampSplitRatio(ratio + step);
    setRatioDraft(next);
    onSplitRatioChange(next);
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
                trashMode
                  ? undefined
                  : (event) => {
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
                    }
              }
              onClick={(event) => selectNode(node, event, pane)}
              onContextMenu={(event) => {
                event.preventDefault();
                event.stopPropagation();
                activePaneRef.current = pane;
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
                  activePaneRef.current = pane;
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
                  activePaneRef.current = pane;
                  setMenu(null);
                  onSelectDirect(node.id);
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

      {menu && trashMode && menuNode ? (
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
            onClick={() => {
              const target = menuNode;
              setMenu(null);
              if (target) void restoreFolderNode(target);
            }}
          >
            <RotateCcw size={14} />
            <span>恢复文件夹</span>
          </button>
          <button
            type="button"
            className="context-menu-item context-menu-danger"
            onClick={() => {
              const target = menuNode;
              setMenu(null);
              if (target) void deleteFolderPermanently(target);
            }}
          >
            <Trash2 size={14} />
            <span>永久删除文件夹</span>
          </button>
        </div>
      ) : null}

      {menu && !trashMode ? (
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
              onClick={() => {
                setMenu(null);
                setDialogError(null);
                setCreateParent("");
              }}
            >
              <FolderPlus size={14} />
              <span>新建文件夹</span>
            </button>
          ) : (
            <>
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
            </>
          )}
          <button
            type="button"
            className="context-menu-item"
            disabled={sortDisabled}
            title={sortDisabled ? "没有需要排序的子文件夹" : undefined}
            onClick={() => {
              setMenu(null);
              setSortError(null);
              if (menuIsRoot) {
                if (tree.rootId != null) setSortTarget({ id: tree.rootId, path: "" });
              } else if (menuNode) {
                setSortTarget({ id: menuNode.id, path: menuNode.path });
              }
            }}
          >
            <ArrowDownUp size={14} />
            <span>排序子文件夹…</span>
          </button>
          {menuIsRoot ? null : (
            <>
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
                onClick={() => {
                  const target = menuNode;
                  setMenu(null);
                  if (target) void deleteFolder(target);
                }}
              >
                <Trash2 size={14} />
                <span>移入回收站</span>
              </button>
            </>
          )}
        </div>
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
