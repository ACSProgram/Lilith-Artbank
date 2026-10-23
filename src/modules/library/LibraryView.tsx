import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
} from "react";
import {
  AlertTriangle,
  ArrowDownUp,
  ArrowLeft,
  CheckSquare,
  ImageOff,
  LayoutGrid,
  List,
  PanelRight,
  RefreshCw,
  Search,
  SlidersHorizontal,
  X,
} from "lucide-react";
import { useAppRuntime } from "../../app/runtimeContext";
import { useNotices } from "../../app/noticeContext";
import { Button, IconButton } from "../../shared/components/Button";
import { Spinner } from "../../shared/components/Spinner";
import { copyText } from "../../shared/clipboard";
import { errorMessage } from "../../shared/errors";
import { libraryApi } from "./api";
import { CollectionManagerDialog } from "./CollectionManagerDialog";
import { CollectionPickerDialog } from "./CollectionPickerDialog";
import { FilterPanel } from "./FilterPanel";
import { InspectorPanel } from "./Inspector";
import { LibraryContextMenu, type ContextMenuState } from "./LibraryContextMenu";
import { TagManagerDialog } from "./TagManagerDialog";
import { ViewBar } from "./ViewBar";
import {
  COLLECTION_SORT,
  DEFAULT_SORT,
  activeFilterCount,
  buildQuery,
  defaultBrowseState,
  filterKey,
  formatDimensions,
  hasActiveFilters,
  hasActiveQuery,
  isFormTarget,
  queryKey,
  selectionRange,
  sortOptionsFor,
  type BrowseState,
} from "./model";
import type {
  CollectionInfo,
  FolderInfo,
  ImageRow,
  LibraryView as LibraryViewType,
  SortKey,
  SortSpec,
  ViewMode,
} from "./types";
import { useImageQuery } from "./useImageQuery";
import { VirtualGrid } from "./VirtualGrid";
import { VirtualList } from "./VirtualList";
import type { VirtualViewHandle } from "./virtual";

interface LibraryViewProps {
  refreshToken: number;
}

/** 涉及文件（文件夹移动、永久删除）的批量上限，与 Rust `MAX_FILE_BATCH_IDS` 保持一致。 */
const MAX_FILE_BATCH = 10_000;

function initialViewMode(): ViewMode {
  try {
    return localStorage.getItem("lilith.viewMode") === "list" ? "list" : "grid";
  } catch {
    return "grid";
  }
}

function sortForView(view: LibraryViewType, sort: SortSpec): SortSpec {
  if (view === "collection") {
    return sort.key === "collectionPosition" ? sort : COLLECTION_SORT;
  }
  return sort.key === "collectionPosition" ? DEFAULT_SORT : sort;
}

export function LibraryView({ refreshToken }: LibraryViewProps) {
  const { settings, current, refreshStatus } = useAppRuntime();
  const { notify, confirm } = useNotices();

  const [browse, setBrowse] = useState<BrowseState>(defaultBrowseState);
  const [searchDraft, setSearchDraft] = useState("");
  const [history, setHistory] = useState<BrowseState[]>([]);
  const [viewMode, setViewMode] = useState<ViewMode>(initialViewMode);
  const [filterOpen, setFilterOpen] = useState(false);
  const [inspectorOpen, setInspectorOpen] = useState(false);
  const [selected, setSelected] = useState<Set<number>>(() => new Set());
  const [activeId, setActiveId] = useState<number | null>(null);
  const [anchorId, setAnchorId] = useState<number | null>(null);
  const [gridColumns, setGridColumns] = useState(1);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [selectingAll, setSelectingAll] = useState(false);
  const [dataVersion, setDataVersion] = useState(0);
  const [collections, setCollections] = useState<CollectionInfo[]>([]);
  const [collectionsLoaded, setCollectionsLoaded] = useState(false);
  const [folders, setFolders] = useState<FolderInfo[]>([]);
  const [collectionManagerOpen, setCollectionManagerOpen] = useState(false);
  const [collectionPickerOpen, setCollectionPickerOpen] = useState(false);
  const [tagManagerOpen, setTagManagerOpen] = useState(false);
  const [emptyingTrash, setEmptyingTrash] = useState(false);
  const [viewBusy, setViewBusy] = useState(false);

  const viewRef = useRef<VirtualViewHandle>(null);
  const filterAnchorRef = useRef<HTMLDivElement>(null);
  const searchInputRef = useRef<HTMLInputElement>(null);
  const browseRef = useRef(browse);
  browseRef.current = browse;
  const historyRef = useRef(history);
  historyRef.current = history;

  const effectiveView: LibraryViewType =
    browse.search && browse.searchScope === "library" ? "all" : browse.view;
  const effectiveCollectionId =
    effectiveView === "collection" ? browse.collectionId : null;

  const query = useMemo(() => buildQuery(browse), [browse]);
  const {
    items,
    status,
    error,
    moreStatus,
    moreError,
    hasMore,
    loadMore,
    retry,
    retryMore,
  } = useImageQuery(query, refreshToken + dataVersion);
  const resetSignature = useMemo(
    () => `${queryKey(query)}|${refreshToken}`,
    [query, refreshToken],
  );
  const selectionResetSignature = useMemo(
    () => `${filterKey(query)}|${refreshToken}`,
    [query, refreshToken],
  );
  const thumbPx = settings?.ui.gridThumbPx ?? 220;

  const clearSelection = useCallback(() => {
    setSelected(new Set());
    setActiveId(null);
    setAnchorId(null);
    setContextMenu(null);
  }, []);

  const refreshData = useCallback(
    (options?: { clearSelection?: boolean }) => {
      if (options?.clearSelection) clearSelection();
      setDataVersion((value) => value + 1);
      void refreshStatus().catch((cause: unknown) => {
        notify("刷新图库状态失败", "error", errorMessage(cause));
      });
    },
    [clearSelection, notify, refreshStatus],
  );

  const collectionsGeneration = useRef(0);
  const reloadCollections = useCallback(async () => {
    const generation = collectionsGeneration.current + 1;
    collectionsGeneration.current = generation;
    try {
      const next = await libraryApi.getCollections();
      if (generation !== collectionsGeneration.current) return;
      setCollections(next);
      setCollectionsLoaded(true);
    } catch (cause) {
      if (generation !== collectionsGeneration.current) return;
      notify("读取集合失败", "error", errorMessage(cause));
    }
  }, [notify]);

  const foldersGeneration = useRef(0);
  const reloadFolders = useCallback(async () => {
    const generation = foldersGeneration.current + 1;
    foldersGeneration.current = generation;
    try {
      const next = await libraryApi.getFolders();
      if (generation !== foldersGeneration.current) return;
      setFolders(next);
    } catch (cause) {
      if (generation !== foldersGeneration.current) return;
      notify("读取文件夹失败", "error", errorMessage(cause));
    }
  }, [notify]);

  useEffect(() => {
    void reloadCollections();
    void reloadFolders();
  }, [reloadCollections, reloadFolders, refreshToken, dataVersion]);

  useEffect(() => {
    setSelected(new Set());
    setActiveId(null);
    setAnchorId(null);
    setContextMenu(null);
  }, [selectionResetSignature]);

  useEffect(() => {
    try {
      localStorage.setItem("lilith.viewMode", viewMode);
    } catch {
      // 忽略无法持久化视图模式的浏览器限制。
    }
  }, [viewMode]);

  useEffect(() => {
    setSearchDraft(browse.search);
  }, [browse.search]);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      const next = searchDraft.trim();
      const currentState = browseRef.current;
      if (next === currentState.search) return;
      applyBrowse({
        ...currentState,
        search: next,
        searchScope: next ? currentState.searchScope : "view",
      });
    }, 260);
    return () => window.clearTimeout(timer);
  }, [searchDraft]);

  useEffect(() => {
    if (!collectionsLoaded || browse.view !== "collection" || browse.collectionId == null) return;
    if (collections.some((collection) => collection.id === browse.collectionId)) return;
    applyBrowse({
      ...browseRef.current,
      view: "all",
      collectionId: null,
      sort: sortForView("all", browseRef.current.sort),
    }, false);
  }, [collections, collectionsLoaded, browse.view, browse.collectionId]);

  useEffect(() => {
    if (!filterOpen) return;
    const onPointerDown = (event: PointerEvent) => {
      if (filterAnchorRef.current && !filterAnchorRef.current.contains(event.target as Node)) {
        setFilterOpen(false);
      }
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [filterOpen]);

  const applyBrowse = useCallback((next: BrowseState, pushHistory = true) => {
    const previous = browseRef.current;
    if (pushHistory && filterKey(buildQuery(previous)) !== filterKey(buildQuery(next))) {
      setHistory((value) => [...value.slice(-29), previous]);
    }
    browseRef.current = next;
    setBrowse(next);
  }, []);

  const goBack = useCallback(() => {
    const previous = historyRef.current;
    if (previous.length === 0) return;
    const next = previous[previous.length - 1];
    setHistory(previous.slice(0, -1));
    browseRef.current = next;
    setBrowse(next);
  }, []);

  // 切换固定视图时搜索范围回到“当前视图”，集合内顺序排序在离开集合时回退到默认排序。
  const showAll = useCallback(() => {
    applyBrowse({
      ...browseRef.current,
      view: "all",
      collectionId: null,
      searchScope: "view",
      sort: sortForView("all", browseRef.current.sort),
    });
  }, [applyBrowse]);

  const showTrash = useCallback(() => {
    applyBrowse({
      ...browseRef.current,
      view: "trash",
      collectionId: null,
      searchScope: "view",
      sort: sortForView("trash", browseRef.current.sort),
    });
  }, [applyBrowse]);

  const showCollection = useCallback(
    (collectionId: number) => {
      const currentState = browseRef.current;
      applyBrowse({
        ...currentState,
        view: "collection",
        collectionId,
        searchScope: "view",
        sort:
          currentState.view === "collection"
            ? currentState.sort
            : COLLECTION_SORT,
      });
    },
    [applyBrowse],
  );

  const setSearchScope = (scope: BrowseState["searchScope"]) => {
    const currentState = browseRef.current;
    applyBrowse({
      ...currentState,
      searchScope: scope,
      sort:
        scope === "library" && currentState.sort.key === "collectionPosition"
          ? DEFAULT_SORT
          : currentState.sort,
    });
  };

  const activateItem = useCallback(
    (item: ImageRow) => {
      setActiveId(item.id);
      setSelected(new Set([item.id]));
      setAnchorId(item.id);
      setInspectorOpen(true);
      notify(
        "查看器将在 Stage 4 提供",
        "info",
        `图片 #${item.id} · ${formatDimensions(item.width, item.height)}`,
      );
    },
    [notify],
  );

  const handleItemClick = useCallback(
    (item: ImageRow, event: ReactMouseEvent) => {
      setContextMenu(null);
      setInspectorOpen(true);
      if (event.shiftKey && anchorId != null) {
        setSelected(selectionRange(items, anchorId, item.id));
        setActiveId(item.id);
        return;
      }
      if (event.ctrlKey || event.metaKey) {
        setSelected((previous) => {
          const next = new Set(previous);
          if (next.has(item.id)) next.delete(item.id);
          else next.add(item.id);
          return next;
        });
        setAnchorId(item.id);
        setActiveId(item.id);
        return;
      }
      setSelected(new Set([item.id]));
      setAnchorId(item.id);
      setActiveId(item.id);
    },
    [anchorId, items],
  );

  const handleItemContextMenu = useCallback(
    (item: ImageRow, event: ReactMouseEvent) => {
      event.preventDefault();
      event.stopPropagation();
      const wasSelected = selected.has(item.id);
      const targetItems = wasSelected ? items.filter((image) => selected.has(image.id)) : [item];
      const ids = wasSelected ? [...selected] : [item.id];
      if (!wasSelected) {
        setSelected(new Set([item.id]));
        setAnchorId(item.id);
      }
      setActiveId(item.id);
      setInspectorOpen(true);
      setContextMenu({
        x: event.clientX,
        y: event.clientY,
        items: targetItems.length > 0 ? targetItems : [item],
        ids: ids.length > 0 ? ids : [item.id],
      });
    },
    [items, selected],
  );

  const handleBackgroundClick = useCallback(() => {
    setContextMenu(null);
    clearSelection();
  }, [clearSelection]);

  const selectAllMatches = useCallback(async () => {
    if (selectingAll) return;
    setSelectingAll(true);
    try {
      const ids = await libraryApi.queryImageIds(query);
      setSelected(new Set(ids));
      const first = ids[0] ?? null;
      setAnchorId(first);
      setActiveId(first);
      if (ids.length > 0) setInspectorOpen(true);
      notify(
        `已选择 ${ids.length.toLocaleString()} 张`,
        "info",
        ids.length === 0 ? "当前条件没有图片" : "包含尚未滚动加载的图片",
      );
    } catch (cause) {
      notify("全选失败", "error", errorMessage(cause));
    } finally {
      setSelectingAll(false);
    }
  }, [notify, query, selectingAll]);

  const copyPaths = useCallback(async () => {
    const targets = contextMenu?.items ?? [];
    if (targets.length === 0) return;
    try {
      await copyText(targets.map((item) => item.filePath).join("\n"));
      const hidden = contextMenu && contextMenu.ids.length > targets.length
        ? contextMenu.ids.length - targets.length
        : 0;
      notify(
        `已复制 ${targets.length.toLocaleString()} 条文件路径`,
        "success",
        hidden > 0
          ? `另有 ${hidden.toLocaleString()} 张未加载，仅复制了已加载项`
          : undefined,
      );
    } catch (cause) {
      notify("复制路径失败", "error", errorMessage(cause));
    }
    setContextMenu(null);
  }, [contextMenu, notify]);

  const copyHashes = useCallback(async () => {
    const targets = contextMenu?.items ?? [];
    if (targets.length === 0) return;
    try {
      await copyText(targets.map((item) => item.contentHash).join("\n"));
      notify(`已复制 ${targets.length.toLocaleString()} 条内容哈希`, "success");
    } catch (cause) {
      notify("复制哈希失败", "error", errorMessage(cause));
    }
    setContextMenu(null);
  }, [contextMenu, notify]);

  const trashSelected = useCallback(
    async (ids: number[]) => {
      if (ids.length === 0 || viewBusy) return;
      setViewBusy(true);
      try {
        const changed = await libraryApi.trashImages(ids);
        notify(`已移入回收站 ${changed.toLocaleString()} 张`, "success");
        refreshData({ clearSelection: true });
      } catch (cause) {
        notify("移入回收站失败", "error", errorMessage(cause));
      } finally {
        setViewBusy(false);
      }
    },
    [notify, refreshData, viewBusy],
  );

  const restoreSelected = useCallback(
    async (ids: number[]) => {
      if (ids.length === 0 || viewBusy) return;
      setViewBusy(true);
      try {
        const changed = await libraryApi.restoreImages(ids);
        notify(`已恢复 ${changed.toLocaleString()} 张`, "success");
        refreshData({ clearSelection: true });
      } catch (cause) {
        notify("恢复失败", "error", errorMessage(cause));
      } finally {
        setViewBusy(false);
      }
    },
    [notify, refreshData, viewBusy],
  );

  const deleteSelected = useCallback(
    async (ids: number[]) => {
      if (ids.length === 0 || viewBusy) return;
      if (ids.length > MAX_FILE_BATCH) {
        notify(
          "一次永久删除的图片过多",
          "error",
          `一次最多 ${MAX_FILE_BATCH.toLocaleString()} 张，请分批处理。`,
        );
        return;
      }
      const accepted = await confirm({
        title: `永久删除 ${ids.length.toLocaleString()} 张图片？`,
        message:
          "会从图库删除记录与原图文件，无法恢复。标签、集合引用与缩略图指针会一并清理。",
        confirmLabel: "永久删除",
        danger: true,
      });
      if (!accepted) return;
      setViewBusy(true);
      try {
        const report = await libraryApi.deleteImagesPermanently(ids);
        const failed = report.failures.length;
        notify(
          `已永久删除 ${report.deleted.toLocaleString()} 张`,
          failed > 0 ? "error" : "success",
          failed > 0 ? `${failed} 个文件删除失败：${report.failures[0].reason}` : undefined,
        );
        refreshData({ clearSelection: true });
      } catch (cause) {
        notify("永久删除失败", "error", errorMessage(cause));
      } finally {
        setViewBusy(false);
      }
    },
    [confirm, notify, refreshData, viewBusy],
  );

  const emptyTrash = useCallback(async () => {
    if (viewBusy) return;
    const count = current?.trashCount ?? 0;
    const accepted = await confirm({
      title: "清空回收站？",
      message: `会永久删除回收站中的 ${count.toLocaleString()} 张图片及其原图文件，无法恢复。`,
      confirmLabel: "清空回收站",
      danger: true,
    });
    if (!accepted) return;
    setEmptyingTrash(true);
    setViewBusy(true);
    try {
      const report = await libraryApi.emptyTrash();
      notify(
        `已清空回收站：删除 ${report.deleted.toLocaleString()} 张`,
        report.failures.length > 0 ? "error" : "success",
        report.failures.length > 0
          ? `${report.failures.length} 个文件删除失败：${report.failures[0].reason}`
          : undefined,
      );
      refreshData({ clearSelection: true });
    } catch (cause) {
      notify("清空回收站失败", "error", errorMessage(cause));
    } finally {
      setEmptyingTrash(false);
      setViewBusy(false);
    }
  }, [confirm, current?.trashCount, notify, refreshData, viewBusy]);

  const removeFromCurrentCollection = useCallback(
    async (ids: number[]) => {
      if (effectiveView !== "collection" || effectiveCollectionId == null || ids.length === 0) {
        return;
      }
      if (viewBusy) return;
      setViewBusy(true);
      try {
        const removed = await libraryApi.removeImagesFromCollection(
          effectiveCollectionId,
          ids,
        );
        notify(`已从当前集合移出 ${removed.toLocaleString()} 张`, "success");
        void reloadCollections();
        refreshData({ clearSelection: true });
      } catch (cause) {
        notify("移出集合失败", "error", errorMessage(cause));
      } finally {
        setViewBusy(false);
      }
    },
    [effectiveCollectionId, effectiveView, notify, refreshData, reloadCollections, viewBusy],
  );

  const handleCollectionAdded = useCallback(() => {
    void reloadCollections();
    refreshData();
  }, [refreshData, reloadCollections]);

  const handleTagChanged = useCallback(() => {
    refreshData();
  }, [refreshData]);

  const columnCount = viewMode === "grid" ? gridColumns : 1;

  const moveActive = useCallback(
    (delta: number, extend: boolean) => {
      if (items.length === 0) return;
      const currentIndex = activeId == null ? -1 : items.findIndex((item) => item.id === activeId);
      const nextIndex =
        currentIndex < 0
          ? 0
          : Math.min(items.length - 1, Math.max(0, currentIndex + delta));
      const next = items[nextIndex];
      if (!next) return;
      setActiveId(next.id);
      setInspectorOpen(true);
      if (extend) {
        if (anchorId == null) {
          setAnchorId(next.id);
          setSelected(new Set([next.id]));
        } else {
          setSelected(selectionRange(items, anchorId, next.id));
        }
      } else {
        setSelected(new Set([next.id]));
        setAnchorId(next.id);
      }
      viewRef.current?.ensureIndex(nextIndex);
      if (nextIndex >= items.length - columnCount * 2 && hasMore) loadMore();
    },
    [activeId, anchorId, columnCount, hasMore, items, loadMore],
  );

  const resetFilters = useCallback(() => {
    applyBrowse({ ...browseRef.current, filters: defaultBrowseState().filters });
  }, [applyBrowse]);

  const handleKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (event.defaultPrevented) return;
    const key = event.key;
    if ((event.ctrlKey || event.metaKey) && key.toLowerCase() === "f") {
      event.preventDefault();
      searchInputRef.current?.focus();
      searchInputRef.current?.select();
      return;
    }
    if (key === "Escape") {
      event.preventDefault();
      if (filterOpen) setFilterOpen(false);
      else if (contextMenu) setContextMenu(null);
      else clearSelection();
      return;
    }
    if (isFormTarget(event.target)) return;
    if (contextMenu) setContextMenu(null);

    if ((event.ctrlKey || event.metaKey) && key.toLowerCase() === "z") {
      event.preventDefault();
      goBack();
      return;
    }
    if ((event.ctrlKey || event.metaKey) && key.toLowerCase() === "a") {
      event.preventDefault();
      void selectAllMatches();
      return;
    }
    if (key === "Enter") {
      const target =
        items.find((item) => item.id === activeId) ?? items.find((item) => selected.has(item.id));
      if (target) {
        event.preventDefault();
        activateItem(target);
      }
      return;
    }
    if (key === "Delete" || key === "Backspace") {
      event.preventDefault();
      const ids = selected.size > 0 ? [...selected] : activeId != null ? [activeId] : [];
      if (ids.length === 0) return;
      if (effectiveView === "trash") void deleteSelected(ids);
      else void trashSelected(ids);
      return;
    }
    if (key === "PageDown") {
      event.preventDefault();
      viewRef.current?.scrollByPages(1);
      return;
    }
    if (key === "PageUp") {
      event.preventDefault();
      viewRef.current?.scrollByPages(-1);
      return;
    }
    if (key === "Home") {
      event.preventDefault();
      viewRef.current?.scrollToTop();
      return;
    }
    if (key === "End") {
      event.preventDefault();
      viewRef.current?.scrollToBottom();
      return;
    }
    if (key === "ArrowRight") {
      event.preventDefault();
      moveActive(1, event.shiftKey);
    } else if (key === "ArrowLeft") {
      event.preventDefault();
      moveActive(-1, event.shiftKey);
    } else if (key === "ArrowDown") {
      event.preventDefault();
      moveActive(columnCount, event.shiftKey);
    } else if (key === "ArrowUp") {
      event.preventDefault();
      moveActive(-columnCount, event.shiftKey);
    }
  };

  const filterSummary = useMemo(() => {
    const parts: string[] = [];
    if (browse.search) {
      parts.push(`搜索「${browse.search}」${browse.searchScope === "library" ? "（全库）" : ""}`);
    }
    if (browse.filters.folderIds.length > 0) parts.push(`文件夹 ${browse.filters.folderIds.length}`);
    if (browse.filters.extensions.length > 0) parts.push(`扩展名 ${browse.filters.extensions.length}`);
    if (browse.filters.scores.length > 0) parts.push(`评分 ${browse.filters.scores.length}`);
    if (browse.filters.tagIds.length > 0) {
      parts.push(
        `标签 ${browse.filters.tagIds.length}（${browse.filters.tagMode === "all" ? "全部" : "任一"}）`,
      );
    }
    if (browse.filters.importedFrom != null || browse.filters.importedTo != null) {
      parts.push("导入时间");
    }
    if (browse.filters.sizeMin != null || browse.filters.sizeMax != null) {
      parts.push("文件大小");
    }
    const presence = [
      ["描述", browse.filters.description],
      ["Embedding", browse.filters.embedding],
      ["Pose", browse.filters.pose],
    ] as const;
    for (const [label, value] of presence) {
      if (value === "has") parts.push(`有${label}`);
      if (value === "missing") parts.push(`无${label}`);
    }
    return parts;
  }, [browse]);

  const filterCount = activeFilterCount(browse.filters);
  const noResults = status === "ready" && items.length === 0;
  const selectedIds = useMemo(() => [...selected], [selected]);
  const contextHasDeleted = contextMenu?.items.some((item) => item.deletedAt != null) ?? false;
  const contextHasActive = contextMenu?.items.some((item) => item.deletedAt == null) ?? false;
  const searchOptions = sortOptionsFor(browse.view).filter(
    (option) => !(browse.searchScope === "library" && option.key === "collectionPosition"),
  );

  return (
    <div className="library-view" onKeyDown={handleKeyDown}>
      <ViewBar
        view={effectiveView}
        collectionId={effectiveCollectionId}
        collections={collections}
        imageCount={current?.imageCount ?? 0}
        trashCount={current?.trashCount ?? 0}
        emptyingTrash={emptyingTrash}
        onShowAll={showAll}
        onShowTrash={showTrash}
        onShowCollection={showCollection}
        onManageCollections={() => setCollectionManagerOpen(true)}
        onEmptyTrash={() => void emptyTrash()}
      />

      <div className="library-toolbar">
        <div className="library-toolbar-left">
          <div className="segmented" role="group" aria-label="视图模式">
            <button
              type="button"
              className={viewMode === "grid" ? "is-active" : ""}
              title="网格视图"
              aria-label="网格视图"
              onClick={() => setViewMode("grid")}
            >
              <LayoutGrid size={15} />
            </button>
            <button
              type="button"
              className={viewMode === "list" ? "is-active" : ""}
              title="列表视图"
              aria-label="列表视图"
              onClick={() => setViewMode("list")}
            >
              <List size={15} />
            </button>
          </div>

          <IconButton
            label="返回上一视图"
            disabled={history.length === 0}
            onClick={goBack}
          >
            <ArrowLeft size={14} />
          </IconButton>

          <div className="search-field">
            <Search size={14} />
            <input
              ref={searchInputRef}
              className="input"
              value={searchDraft}
              placeholder="搜索描述、标签、文件夹"
              aria-label="搜索图片"
              onChange={(event) => setSearchDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  // 只清空搜索框，不触发容器的“清空选择”。
                  event.stopPropagation();
                  setSearchDraft("");
                  event.currentTarget.blur();
                }
              }}
            />
            {searchDraft ? (
              <IconButton label="清除搜索" onClick={() => setSearchDraft("")}>
                <X size={12} />
              </IconButton>
            ) : null}
            {searchDraft.trim() ? (
              <button
                type="button"
                className={`search-scope${
                  browse.searchScope === "library" ? " is-active" : ""
                }`}
                title={
                  browse.searchScope === "library"
                    ? "当前搜索包含回收站；点击改为只搜索当前视图"
                    : "当前只在当前视图内搜索；点击搜索全库（含回收站）"
                }
                onClick={() =>
                  setSearchScope(browse.searchScope === "library" ? "view" : "library")
                }
              >
                {browse.searchScope === "library" ? "全库" : "当前视图"}
              </button>
            ) : null}
          </div>

          <span className="library-loaded">
            {status === "loading" ? "正在读取…" : `${items.length.toLocaleString()} 张已载入`}
          </span>
          {hasMore ? <span className="library-loaded-hint">继续滚动载入更多</span> : null}
        </div>

        <div className="library-toolbar-right">
          {selected.size > 0 ? (
            <div className="selection-summary">
              {selectingAll ? <Spinner size={13} /> : <CheckSquare size={14} />}
              <span>已选择 {selected.size.toLocaleString()} 张</span>
              <IconButton label="清除选择" onClick={clearSelection}>
                <X size={13} />
              </IconButton>
            </div>
          ) : null}

          <IconButton
            label={inspectorOpen ? "隐藏详情侧栏" : "显示详情侧栏"}
            className={inspectorOpen && selected.size > 0 ? "is-active" : ""}
            disabled={selected.size === 0}
            onClick={() => setInspectorOpen((value) => !value)}
          >
            <PanelRight size={14} />
          </IconButton>

          <div className="filter-anchor" ref={filterAnchorRef}>
            <Button
              size="sm"
              variant={filterCount > 0 ? "primary" : "secondary"}
              icon={<SlidersHorizontal size={14} />}
              onClick={() => setFilterOpen((open) => !open)}
            >
              筛选
              {filterCount > 0 ? ` · ${filterCount}` : ""}
            </Button>
            <FilterPanel
              open={filterOpen}
              filters={browse.filters}
              query={query}
              dataVersion={dataVersion}
              onFiltersChange={(filters) =>
                applyBrowse({ ...browseRef.current, filters })
              }
              onClose={() => setFilterOpen(false)}
              onReset={resetFilters}
              onManageTags={() => setTagManagerOpen(true)}
            />
          </div>

          <div className="sort-control">
            <select
              className="input sort-select"
              value={browse.sort.key}
              aria-label="排序方式"
              onChange={(event) => {
                const key = event.target.value as SortKey;
                const descending =
                  key === "collectionPosition"
                    ? false
                    : browse.sort.key === "collectionPosition"
                      ? true
                      : browse.sort.descending;
                applyBrowse(
                  { ...browseRef.current, sort: { key, descending } },
                  false,
                );
              }}
            >
              {searchOptions.map((option: { key: SortKey; label: string }) => (
                <option key={option.key} value={option.key}>
                  {option.label}
                </option>
              ))}
            </select>
            <IconButton
              label={browse.sort.descending ? "降序，点击切换为升序" : "升序，点击切换为降序"}
              onClick={() =>
                applyBrowse(
                  {
                    ...browseRef.current,
                    sort: { ...browse.sort, descending: !browse.sort.descending },
                  },
                  false,
                )
              }
            >
              <ArrowDownUp size={14} />
            </IconButton>
          </div>

          <Button size="sm" variant="ghost" icon={<RefreshCw size={14} />} onClick={retry}>
            刷新
          </Button>
        </div>
      </div>

      {filterSummary.length > 0 ? (
        <div className="filter-summary">
          <span>{filterSummary.join(" · ")}</span>
          {hasActiveFilters(browse.filters) ? (
            <button type="button" className="text-button" onClick={resetFilters}>
              清除筛选
            </button>
          ) : null}
          {browse.search ? (
            <button type="button" className="text-button" onClick={() => setSearchDraft("")}>
              清除搜索
            </button>
          ) : null}
        </div>
      ) : null}

      <div className="library-main">
        <div className="library-canvas">
          {status === "loading" && items.length === 0 ? (
            <div className="library-state">
              <Spinner size={22} />
              <strong>正在读取图片</strong>
              <span>正在查询首屏元数据…</span>
            </div>
          ) : null}

          {status === "error" ? (
            <div className="library-state library-state-error">
              <AlertTriangle size={24} />
              <strong>图片列表读取失败</strong>
              <span>{error}</span>
              <Button size="sm" variant="primary" onClick={retry}>
                重试
              </Button>
            </div>
          ) : null}

          {noResults ? (
            <div className="library-state">
              <ImageOff size={24} />
              <strong>
                {effectiveView === "trash"
                  ? "回收站是空的"
                  : hasActiveQuery(browse)
                    ? "没有符合条件的图片"
                    : "当前视图没有可显示的图片"}
              </strong>
              <span>
                {hasActiveQuery(browse)
                  ? "调整搜索或筛选条件，或者清除后查看全部图片。"
                  : effectiveView === "trash"
                    ? "从图库移入回收站的图片会显示在这里，可以恢复或永久删除。"
                    : effectiveView === "collection"
                      ? "把选中的图片加入这个集合后，它们会出现在这里。"
                      : "导入图片后会显示在这里。"}
              </span>
              {hasActiveFilters(browse.filters) ? (
                <Button size="sm" variant="primary" onClick={resetFilters}>
                  清除筛选
                </Button>
              ) : browse.search ? (
                <Button size="sm" variant="primary" onClick={() => setSearchDraft("")}>
                  清除搜索
                </Button>
              ) : null}
            </div>
          ) : null}

          {items.length > 0 && viewMode === "grid" ? (
            <VirtualGrid
              ref={viewRef}
              items={items}
              thumbPx={thumbPx}
              resetKey={resetSignature}
              selected={selected}
              activeId={activeId}
              hasMore={hasMore}
              moreStatus={moreStatus}
              moreError={moreError}
              onLoadMore={loadMore}
              onRetryMore={retryMore}
              onItemClick={handleItemClick}
              onItemActivate={activateItem}
              onItemContextMenu={handleItemContextMenu}
              onBackgroundClick={handleBackgroundClick}
              onLayout={setGridColumns}
            />
          ) : null}

          {items.length > 0 && viewMode === "list" ? (
            <VirtualList
              ref={viewRef}
              items={items}
              resetKey={resetSignature}
              selected={selected}
              activeId={activeId}
              hasMore={hasMore}
              moreStatus={moreStatus}
              moreError={moreError}
              onLoadMore={loadMore}
              onRetryMore={retryMore}
              onItemClick={handleItemClick}
              onItemActivate={activateItem}
              onItemContextMenu={handleItemContextMenu}
              onBackgroundClick={handleBackgroundClick}
            />
          ) : null}
        </div>

        {inspectorOpen && selectedIds.length > 0 ? (
          <InspectorPanel
            selectedIds={selectedIds}
            version={dataVersion}
            folders={folders}
            currentCollectionId={effectiveCollectionId}
            onClose={() => setInspectorOpen(false)}
            onChanged={refreshData}
            onAddToCollection={() => setCollectionPickerOpen(true)}
            onRemoveFromCollection={(ids) => void removeFromCurrentCollection(ids)}
            onTrash={(ids) => void trashSelected(ids)}
            onRestore={(ids) => void restoreSelected(ids)}
            onDelete={(ids) => void deleteSelected(ids)}
          />
        ) : null}
      </div>

      {contextMenu ? (
        <LibraryContextMenu
          menu={contextMenu}
          inCollection={effectiveView === "collection"}
          hasActive={contextHasActive}
          hasDeleted={contextHasDeleted}
          onClose={() => setContextMenu(null)}
          onOpenViewer={() => {
            const item = contextMenu.items[0];
            setContextMenu(null);
            if (item) activateItem(item);
          }}
          onShowDetails={() => {
            setInspectorOpen(true);
            setContextMenu(null);
          }}
          onCopyPaths={() => void copyPaths()}
          onCopyHashes={() => void copyHashes()}
          onAddToCollection={() => {
            setContextMenu(null);
            setCollectionPickerOpen(true);
          }}
          onRemoveFromCollection={() => {
            const ids = contextMenu.ids;
            setContextMenu(null);
            void removeFromCurrentCollection(ids);
          }}
          onTrash={() => {
            const ids = contextMenu.ids;
            setContextMenu(null);
            void trashSelected(ids);
          }}
          onRestore={() => {
            const ids = contextMenu.ids;
            setContextMenu(null);
            void restoreSelected(ids);
          }}
          onDelete={() => {
            const ids = contextMenu.ids;
            setContextMenu(null);
            void deleteSelected(ids);
          }}
          onSelectAllMatches={() => {
            setContextMenu(null);
            void selectAllMatches();
          }}
        />
      ) : null}

      <CollectionManagerDialog
        open={collectionManagerOpen}
        collections={collections}
        onClose={() => setCollectionManagerOpen(false)}
        onChanged={() => void reloadCollections()}
      />
      <CollectionPickerDialog
        open={collectionPickerOpen}
        imageIds={selectedIds}
        collections={collections}
        onClose={() => setCollectionPickerOpen(false)}
        onAdded={handleCollectionAdded}
      />
      <TagManagerDialog
        open={tagManagerOpen}
        onClose={() => setTagManagerOpen(false)}
        onChanged={handleTagChanged}
      />
    </div>
  );
}
