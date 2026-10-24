import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type DragEvent as ReactDragEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
} from "react";
import {
  AlertTriangle,
  ArrowDownUp,
  ArrowLeft,
  CheckSquare,
  ChevronRight,
  FolderOpen,
  ImageOff,
  LayoutGrid,
  List,
  PanelRight,
  RefreshCw,
  ScanSearch,
  Search,
  SlidersHorizontal,
  Sparkles,
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
import { IMAGE_DRAG_MIME } from "./dnd";
import { breadcrumbsFor, buildFolderTree } from "./folderTreeModel";
import {
  COLLECTION_SORT,
  DEFAULT_SORT,
  activeFilterCount,
  buildQuery,
  emptyFilters,
  filterKey,
  hasActiveFilters,
  hasActiveQuery,
  isFormTarget,
  queryKey,
  selectionRange,
  sortOptionsFor,
  type BrowseState,
  type SimilarityBrowseState,
} from "./model";
import type {
  CollectionInfo,
  FolderInfo,
  ImageRow,
  LibraryQuery,
  LibraryView as LibraryViewType,
  SortKey,
  SortSpec,
  ViewMode,
} from "./types";
import { useImageQuery } from "./useImageQuery";
import { VirtualGrid } from "./VirtualGrid";
import { VirtualList } from "./VirtualList";
import type { VirtualViewHandle } from "./virtual";
import type { ViewerSession } from "../../shared/viewerSession";
import type { SimilarKind } from "../../shared/similarity";
import { SIMILAR_LABELS } from "../../shared/similarity";

interface LibraryViewProps {
  browse: BrowseState;
  canGoBack: boolean;
  folders: FolderInfo[];
  onApplyBrowse: (next: BrowseState, pushHistory?: boolean) => void;
  onGoBack: () => void;
  onFoldersChanged?: () => void;
  onImageSelectionChange?: (imageIds: number[]) => void;
  refreshToken: number;
  onOpenViewer: (session: ViewerSession) => void;
  onCopyImage: (imageId: number) => void;
  similarity?: SimilarityBrowseState;
  onFindSimilar?: (kind: SimilarKind, targetId: number) => void;
  onOpenSimilarityGroups?: () => void;
  onOpenAiTasks?: (context: { selectedIds: number[]; query: LibraryQuery | null }) => void;
  onGenerateDescription?: (imageId: number) => Promise<{ text: string; model: string }>;
  aiOnline?: boolean;
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

export function LibraryView({
  browse,
  canGoBack,
  folders,
  onApplyBrowse,
  onGoBack,
  onFoldersChanged,
  onImageSelectionChange,
  refreshToken,
  onOpenViewer,
  onCopyImage,
  similarity,
  onFindSimilar,
  onOpenSimilarityGroups,
  onOpenAiTasks,
  onGenerateDescription,
  aiOnline = false,
}: LibraryViewProps) {
  const { settings, current, refreshStatus } = useAppRuntime();
  const { notify, confirm } = useNotices();

  const [searchDraft, setSearchDraft] = useState(browse.search);
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
  const [collectionManagerOpen, setCollectionManagerOpen] = useState(false);
  const [collectionPickerOpen, setCollectionPickerOpen] = useState(false);
  const [tagManagerOpen, setTagManagerOpen] = useState(false);
  const [emptyingTrash, setEmptyingTrash] = useState(false);
  const [viewBusy, setViewBusy] = useState(false);
  const [semanticOpen, setSemanticOpen] = useState(false);
  const [semanticDraft, setSemanticDraft] = useState("");

  const viewRef = useRef<VirtualViewHandle>(null);
  const filterAnchorRef = useRef<HTMLDivElement>(null);
  const searchInputRef = useRef<HTMLInputElement>(null);
  const browseRef = useRef(browse);
  browseRef.current = browse;
  const applyBrowse = onApplyBrowse;

  const folderTree = useMemo(() => buildFolderTree(folders), [folders]);
  const breadcrumbs = useMemo(
    () => breadcrumbsFor(folderTree, browse.filters.folderIds),
    [folderTree, browse.filters.folderIds],
  );

  const effectiveView: LibraryViewType =
    browse.search && browse.searchScope === "library" ? "all" : browse.view;
  const effectiveCollectionId =
    effectiveView === "collection" ? browse.collectionId : null;

  const query = useMemo(() => buildQuery(browse), [browse]);
  const imageQuery = useImageQuery(query, refreshToken + dataVersion);
  const similarityActive = similarity?.active === true;
  // 结果来自“找相似”而不是文本语义搜索；状态文案按它分支。
  const similarMode = similarity?.mode === "similar";
  // 相似模式下网格换成结果数据源；两者共享 items/status/error/hasMore 契约。
  const listSource = similarityActive && similarity ? similarity : imageQuery;
  const { items, status, error, moreStatus, moreError, hasMore } = listSource;
  const loadMore = listSource.loadMore;
  const retry = listSource.retry;
  const retryMore = similarityActive && similarity ? similarity.loadMore : imageQuery.retryMore;
  const similarityKey = similarityActive ? `similarity:${similarity?.token ?? ""}` : "";
  const resetSignature = useMemo(
    () => `${queryKey(query)}|${refreshToken}|${similarityKey}`,
    [query, refreshToken, similarityKey],
  );
  const selectionResetSignature = useMemo(
    () => `${filterKey(query)}|${refreshToken}|${similarityKey}`,
    [query, refreshToken, similarityKey],
  );
  const thumbPx = settings?.ui.gridThumbPx ?? 220;

  const itemsRef = useRef(items);
  itemsRef.current = items;
  const hasMoreRef = useRef(hasMore);
  hasMoreRef.current = hasMore;
  const viewerListeners = useRef(new Set<() => void>());

  useEffect(() => {
    for (const listener of viewerListeners.current) listener();
  }, [items]);

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
      onFoldersChanged?.();
      void refreshStatus().catch((cause: unknown) => {
        notify("刷新图库状态失败", "error", errorMessage(cause));
      });
    },
    [clearSelection, notify, onFoldersChanged, refreshStatus],
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

  useEffect(() => {
    void reloadCollections();
  }, [reloadCollections, refreshToken, dataVersion]);

  useEffect(() => {
    setSelected(new Set());
    setActiveId(null);
    setAnchorId(null);
    setContextMenu(null);
  }, [selectionResetSignature]);

  // 外部刷新（导入、批处理完成、元数据变化）会让相似结果过期，直接退出相似模式。
  const similarityClear = similarity?.clear;
  const similarityRefreshRef = useRef(refreshToken + dataVersion);
  useEffect(() => {
    if (similarityRefreshRef.current === refreshToken + dataVersion) return;
    similarityRefreshRef.current = refreshToken + dataVersion;
    similarityClear?.();
  }, [refreshToken, dataVersion, similarityClear]);

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

  const goBack = useCallback(() => {
    onGoBack();
    similarityClear?.();
  }, [onGoBack, similarityClear]);

  // 切换固定视图时搜索范围回到“当前视图”，集合内顺序排序在离开集合时回退到默认排序。
  const showAll = useCallback(() => {
    applyBrowse({
      ...browseRef.current,
      view: "all",
      collectionId: null,
      searchScope: "view",
      sort: sortForView("all", browseRef.current.sort),
    });
    similarityClear?.();
  }, [applyBrowse, similarityClear]);

  const showTrash = useCallback(() => {
    applyBrowse({
      ...browseRef.current,
      view: "trash",
      collectionId: null,
      searchScope: "view",
      sort: sortForView("trash", browseRef.current.sort),
    });
    similarityClear?.();
  }, [applyBrowse, similarityClear]);

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
      similarityClear?.();
    },
    [applyBrowse, similarityClear],
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

  useEffect(() => {
    onImageSelectionChange?.([...selected]);
  }, [selected, onImageSelectionChange]);

  const handleItemDragStart = useCallback(
    (item: ImageRow, event: ReactDragEvent<HTMLDivElement>) => {
      const ids = selected.has(item.id) ? [...selected] : [item.id];
      event.dataTransfer.setData(IMAGE_DRAG_MIME, JSON.stringify({ imageIds: ids }));
      event.dataTransfer.effectAllowed = "move";
    },
    [selected],
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
      const ids =
        similarityActive && similarity
          ? await similarity.ids()
          : await libraryApi.queryImageIds(query);
      setSelected(new Set(ids));
      const first = ids[0] ?? null;
      setAnchorId(first);
      setActiveId(first);
      if (ids.length > 0) setInspectorOpen(true);
      notify(
        `已选择 ${ids.length.toLocaleString()} 张`,
        "info",
        ids.length === 0
          ? "当前条件没有图片"
          : similarityActive
            ? "包含相似结果中尚未滚动加载的图片"
            : "包含尚未滚动加载的图片",
      );
    } catch (cause) {
      notify("全选失败", "error", errorMessage(cause));
    } finally {
      setSelectingAll(false);
    }
  }, [notify, query, selectingAll, similarity, similarityActive]);

  const createCollectionFromCurrent = useCallback(
    async (name: string) => {
      const ids =
        similarityActive && similarity
          ? await similarity.ids()
          : await libraryApi.queryImageIds(query);
      if (ids.length === 0) throw new Error("当前结果没有图片");
      const collection = await libraryApi.createCollection(name);
      await libraryApi.addImagesToCollection(collection.id, ids);
      await reloadCollections();
    },
    [query, reloadCollections, similarity, similarityActive],
  );

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
    async (ids: number[]): Promise<boolean> => {
      if (ids.length === 0 || viewBusy) return false;
      setViewBusy(true);
      try {
        const changed = await libraryApi.trashImages(ids);
        notify(`已移入回收站 ${changed.toLocaleString()} 张`, "success");
        refreshData({ clearSelection: true });
        return changed > 0;
      } catch (cause) {
        notify("移入回收站失败", "error", errorMessage(cause));
        return false;
      } finally {
        setViewBusy(false);
      }
    },
    [notify, refreshData, viewBusy],
  );

  const restoreSelected = useCallback(
    async (ids: number[]): Promise<boolean> => {
      if (ids.length === 0 || viewBusy) return false;
      setViewBusy(true);
      try {
        const changed = await libraryApi.restoreImages(ids);
        notify(`已恢复 ${changed.toLocaleString()} 张`, "success");
        refreshData({ clearSelection: true });
        return changed > 0;
      } catch (cause) {
        notify("恢复失败", "error", errorMessage(cause));
        return false;
      } finally {
        setViewBusy(false);
      }
    },
    [notify, refreshData, viewBusy],
  );

  const deleteSelected = useCallback(
    async (ids: number[]): Promise<boolean> => {
      if (ids.length === 0 || viewBusy) return false;
      if (ids.length > MAX_FILE_BATCH) {
        notify(
          "一次永久删除的图片过多",
          "error",
          `一次最多 ${MAX_FILE_BATCH.toLocaleString()} 张，请分批处理。`,
        );
        return false;
      }
      const accepted = await confirm({
        title: `永久删除 ${ids.length.toLocaleString()} 张图片？`,
        message:
          "会从图库删除记录与原图文件，无法恢复。标签、集合引用与缩略图指针会一并清理。",
        confirmLabel: "永久删除",
        danger: true,
      });
      if (!accepted) return false;
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
        return report.deleted > 0;
      } catch (cause) {
        notify("永久删除失败", "error", errorMessage(cause));
        return false;
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
    async (ids: number[]): Promise<boolean> => {
      if (effectiveView !== "collection" || effectiveCollectionId == null || ids.length === 0) {
        return false;
      }
      if (viewBusy) return false;
      setViewBusy(true);
      try {
        const removed = await libraryApi.removeImagesFromCollection(
          effectiveCollectionId,
          ids,
        );
        notify(`已从当前集合移出 ${removed.toLocaleString()} 张`, "success");
        void reloadCollections();
        refreshData({ clearSelection: true });
        return removed > 0;
      } catch (cause) {
        notify("移出集合失败", "error", errorMessage(cause));
        return false;
      } finally {
        setViewBusy(false);
      }
    },
    [effectiveCollectionId, effectiveView, notify, refreshData, reloadCollections, viewBusy],
  );

  const activateItem = useCallback(
    (item: ImageRow) => {
      setActiveId(item.id);
      setSelected(new Set([item.id]));
      setAnchorId(item.id);
      setContextMenu(null);
      const session: ViewerSession = {
        initialId: item.id,
        folders,
        currentCollectionId: effectiveCollectionId,
        getItems: () => itemsRef.current,
        hasMore: () => hasMoreRef.current,
        subscribe: (listener) => {
          viewerListeners.current.add(listener);
          return () => {
            viewerListeners.current.delete(listener);
          };
        },
        loadMore,
        onActiveChange: (imageId) => {
          setActiveId(imageId);
          setSelected(new Set([imageId]));
          setAnchorId(imageId);
          const position = itemsRef.current.findIndex((candidate) => candidate.id === imageId);
          if (position >= 0) viewRef.current?.ensureIndex(position);
        },
        onSaved: () => refreshData(),
        refresh: () => refreshData(),
        addToCollection: (ids) => {
          setSelected(new Set(ids));
          setCollectionPickerOpen(true);
        },
        removeFromCollection: (ids) => removeFromCurrentCollection(ids),
        trash: trashSelected,
        restore: restoreSelected,
        delete: deleteSelected,
      };
      onOpenViewer(session);
    },
    [
      deleteSelected,
      effectiveCollectionId,
      folders,
      loadMore,
      onOpenViewer,
      refreshData,
      removeFromCurrentCollection,
      restoreSelected,
      trashSelected,
    ],
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
    applyBrowse({ ...browseRef.current, filters: emptyFilters() });
  }, [applyBrowse]);

  const showFolder = useCallback(
    (folderId: number | null) => {
      const currentState = browseRef.current;
      applyBrowse({
        ...currentState,
        filters: {
          ...currentState.filters,
          folderIds: folderId == null ? [] : [folderId],
        },
      });
    },
    [applyBrowse],
  );

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

      <div className="folder-breadcrumbs" aria-label="当前文件夹">
        <button
          type="button"
          className={`folder-crumb${browse.filters.folderIds.length === 0 ? " is-current" : ""}`}
          onClick={() => showFolder(null)}
        >
          <FolderOpen size={13} />
          全部图片
        </button>
        {browse.filters.folderIds.length > 1 ? (
          <>
            <ChevronRight size={12} className="folder-crumb-separator" />
            <span className="folder-crumb is-current">
              已选择 {browse.filters.folderIds.length.toLocaleString()} 个文件夹
            </span>
            <button
              type="button"
              className="text-button"
              onClick={() =>
                applyBrowse({
                  ...browseRef.current,
                  filters: { ...browseRef.current.filters, folderIds: [] },
                })
              }
            >
              清除
            </button>
          </>
        ) : (
          breadcrumbs.slice(1).map((crumb, index, trail) => (
            <span key={crumb.path} className="folder-crumb-group">
              <ChevronRight size={12} className="folder-crumb-separator" />
              <button
                type="button"
                className={`folder-crumb${index === trail.length - 1 ? " is-current" : ""}`}
                onClick={() => showFolder(crumb.id)}
                disabled={crumb.id == null}
                title={crumb.path}
              >
                {crumb.label}
              </button>
            </span>
          ))
        )}
      </div>

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
            disabled={!canGoBack}
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
              disabled={similarityActive}
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

          <Button
            size="sm"
            variant={similarityActive ? "primary" : "secondary"}
            icon={<Sparkles size={14} />}
            disabled={!similarity}
            onClick={() => setSemanticOpen((open) => !open)}
          >
            语义搜索
          </Button>

          <Button
            size="sm"
            variant="secondary"
            icon={<ScanSearch size={14} />}
            disabled={!onOpenSimilarityGroups}
            title="按 pHash 或 embedding 扫描全库相似组"
            onClick={() => onOpenSimilarityGroups?.()}
          >
            相似分组
          </Button>

          <span className="library-loaded">
            {similarityActive && similarity?.busy
              ? similarMode
                ? "正在查找相似图片…"
                : "正在计算语义相似度…"
              : status === "loading"
                ? "正在读取…"
                : `${items.length.toLocaleString()} 张已载入`}
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

          <Button
            size="sm"
            variant="secondary"
            icon={<Sparkles size={14} />}
            disabled={!onOpenAiTasks}
            title={aiOnline ? "AI 服务在线" : "AI 服务离线；任务会提示连接状态"}
            onClick={() =>
              onOpenAiTasks?.({
                selectedIds: [...selected],
                query: similarityActive ? null : query,
              })
            }
          >
            AI 任务
          </Button>

          <div className="filter-anchor" ref={filterAnchorRef}>
            <Button
              size="sm"
              variant={filterCount > 0 ? "primary" : "secondary"}
              icon={<SlidersHorizontal size={14} />}
              disabled={similarityActive}
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
              disabled={similarityActive}
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
              disabled={similarityActive}
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

      {semanticOpen && similarity ? (
        <div className="semantic-search-bar">
          <Sparkles size={14} />
          <input
            className="input"
            value={semanticDraft}
            placeholder="描述想找的画面，例如：红色斗篷的侧身人物"
            aria-label="语义搜索"
            autoFocus
            onChange={(event) => setSemanticDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                similarity.search(semanticDraft);
                setSemanticOpen(false);
              }
              if (event.key === "Escape") {
                event.stopPropagation();
                setSemanticOpen(false);
              }
            }}
          />
          <Button
            size="sm"
            variant="primary"
            loading={similarity.busy}
            disabled={!semanticDraft.trim()}
            onClick={() => {
              similarity.search(semanticDraft);
              setSemanticOpen(false);
            }}
          >
            搜索
          </Button>
          <span className="semantic-hint">
            {similarity.error ??
              (similarity.busy
                ? "正在请求文本 embedding 并扫描本地向量…"
                : "文本只用于生成查询向量，不写入图库。")}
          </span>
          <IconButton label="关闭语义搜索" onClick={() => setSemanticOpen(false)}>
            <X size={13} />
          </IconButton>
        </div>
      ) : null}

      {similarityActive && similarity ? (
        <div className="filter-summary semantic-summary">
          {similarMode ? <ScanSearch size={14} /> : <Sparkles size={14} />}
          <span>
            {similarMode
              ? `相似于 ${similarity.text} · ${
                  similarity.kind ? SIMILAR_LABELS[similarity.kind] : "相似"
                }`
              : `语义搜索「${similarity.text}」`}{" "}
            · {similarity.total.toLocaleString()} 张结果
            {!similarMode && similarity.model ? ` · ${similarity.model}` : ""} ·{" "}
            {similarity.elapsedMs}ms
          </span>
          <button type="button" className="text-button" onClick={() => similarity.clear()}>
            退出相似结果
          </button>
        </div>
      ) : null}

      {filterSummary.length > 0 && !similarityActive ? (
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
              <strong>
                {similarityActive
                  ? similarMode
                    ? "正在查找相似图片"
                    : "正在计算语义相似度"
                  : "正在读取图片"}
              </strong>
              <span>
                {similarityActive
                  ? similarMode
                    ? "正在扫描本地 pHash、主色或特征包…"
                    : "正在生成文本 embedding 并扫描本地向量包…"
                  : "正在查询首屏元数据…"}
              </span>
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
                {similarityActive
                  ? similarMode
                    ? "没有找到相似的图片"
                    : "没有语义匹配的图片"
                  : effectiveView === "trash"
                    ? "回收站是空的"
                    : hasActiveQuery(browse)
                      ? "没有符合条件的图片"
                      : "当前视图没有可显示的图片"}
              </strong>
              <span>
                {similarityActive
                  ? similarMode
                    ? "这张图片可能还没有可比较的数据（主色/embedding/姿势），或者阈值过严。"
                    : "换一种描述，或者先为图片生成 embedding 后再试。"
                  : hasActiveQuery(browse)
                    ? "调整搜索或筛选条件，或者清除后查看全部图片。"
                    : effectiveView === "trash"
                      ? "从图库移入回收站的图片会显示在这里，可以恢复或永久删除。"
                      : effectiveView === "collection"
                        ? "把选中的图片加入这个集合后，它们会出现在这里。"
                        : "导入图片后会显示在这里。"}
              </span>
              {similarityActive ? (
                <Button size="sm" variant="primary" onClick={() => similarity?.clear()}>
                  退出相似结果
                </Button>
              ) : hasActiveFilters(browse.filters) ? (
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
              onItemDragStart={handleItemDragStart}
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
              onItemDragStart={handleItemDragStart}
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
            onGenerateDescription={onGenerateDescription}
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
          onCopyImage={() => {
            const item = contextMenu.items[0];
            setContextMenu(null);
            if (item) onCopyImage(item.id);
          }}
          onFindSimilar={(kind) => {
            const item = contextMenu.items[0];
            setContextMenu(null);
            if (item) onFindSimilar?.(kind, item.id);
          }}
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
        currentLabel={similarityActive ? "相似结果" : "当前筛选结果"}
        onCreateFromCurrent={createCollectionFromCurrent}
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
