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
  ChevronRight,
  Crop,
  FolderOpen,
  ImageOff,
  LayoutGrid,
  List,
  Maximize,
  PanelRight,
  RefreshCw,
  ScanSearch,
  Search,
  SlidersHorizontal,
  Sparkles,
  Upload,
  Wrench,
  X,
} from "lucide-react";
import { useAppRuntime } from "../../app/runtimeContext";
import { useNotices } from "../../app/noticeContext";
import { Button, IconButton } from "../../shared/components/Button";
import { MenuSection } from "../../shared/components/MenuSection";
import { Popover } from "../../shared/components/Popover";
import { Spinner } from "../../shared/components/Spinner";
import { copyText } from "../../shared/clipboard";
import { confirmDelete, confirmEmptyTrash, confirmTrash } from "../../shared/confirm";
import { errorMessage } from "../../shared/errors";
import type { ThumbFit, UiSettings } from "../../app/types";
import { libraryApi } from "./api";
import { CollectionManagerDialog } from "./CollectionManagerDialog";
import { CollectionPickerDialog } from "./CollectionPickerDialog";
import {
  CombineCollectionsDialog,
  CreateCollectionDialog,
  RenameCollectionDialog,
} from "./CollectionDialogs";
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
import type { BatchTaskContextInfo } from "../../shared/batchTaskContext";
import type {
  CollectionInfo,
  FeatureSummary,
  FolderInfo,
  ImageCapabilities,
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
  onOpenImport?: () => void;
  similarity?: SimilarityBrowseState;
  onFindSimilar?: (kind: SimilarKind, targetId: number) => void;
  onOpenSimilarityGroups?: () => void;
  onOpenBatchTasks?: (context: BatchTaskContextInfo) => void;
  onGenerateDescription?: (imageId: number) => Promise<{ text: string; model: string }>;
  /** 语义搜索不可用的原因；为空表示可用。 */
  semanticBlockedReason?: string | null;
}

/** 涉及文件（文件夹移动、永久删除）的批量上限，与 Rust `MAX_FILE_BATCH_IDS` 保持一致。 */
const MAX_FILE_BATCH = 10_000;

const THUMB_FITS: { value: ThumbFit; label: string; icon: typeof Crop }[] = [
  { value: "cover", label: "裁剪铺满", icon: Crop },
  { value: "contain", label: "完整显示", icon: Maximize },
];

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
  onOpenImport,
  similarity,
  onFindSimilar,
  onOpenSimilarityGroups,
  onOpenBatchTasks,
  onGenerateDescription,
  semanticBlockedReason = null,
}: LibraryViewProps) {
  const { settings, current, refreshStatus, saveSettings } = useAppRuntime();
  const { notify, confirm } = useNotices();

  const [searchDraft, setSearchDraft] = useState(browse.search);
  const [viewMode, setViewMode] = useState<ViewMode>(initialViewMode);
  const [filterOpen, setFilterOpen] = useState(false);
  const [inspectorOpen, setInspectorOpen] = useState(true);
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
  const [createCollectionOpen, setCreateCollectionOpen] = useState(false);
  const [createPreset, setCreatePreset] = useState<CollectionInfo | null>(null);
  const [renameTarget, setRenameTarget] = useState<CollectionInfo | null>(null);
  const [combineOpen, setCombineOpen] = useState(false);
  const [viewCount, setViewCount] = useState<number | null>(null);
  const [taskMenuOpen, setTaskMenuOpen] = useState(false);
  const [tagManagerOpen, setTagManagerOpen] = useState(false);
  const [emptyingTrash, setEmptyingTrash] = useState(false);
  const [viewBusy, setViewBusy] = useState(false);
  const [semanticOpen, setSemanticOpen] = useState(false);
  const [semanticDraft, setSemanticDraft] = useState("");
  const [contextCaps, setContextCaps] = useState<ImageCapabilities | null>(null);
  const [featureSummary, setFeatureSummary] = useState<FeatureSummary | null>(null);

  const viewRef = useRef<VirtualViewHandle>(null);
  const contextCapsTarget = useRef<number | null>(null);
  const filterAnchorRef = useRef<HTMLDivElement>(null);
  const taskAnchorRef = useRef<HTMLButtonElement>(null);
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
  const thumbFit: ThumbFit = settings?.ui.thumbFit ?? "cover";

  const itemsRef = useRef(items);
  itemsRef.current = items;
  const hasMoreRef = useRef(hasMore);
  hasMoreRef.current = hasMore;
  const viewerListeners = useRef(new Set<() => void>());

  useEffect(() => {
    for (const listener of viewerListeners.current) listener();
  }, [items]);

  /** 界面偏好（填充方式、详情面板）改了就存，不弹“设置已保存”。 */
  const persistUi = useCallback(
    (update: Partial<UiSettings>) => {
      if (!settings) return;
      void saveSettings(
        { ...settings, ui: { ...settings.ui, ...update } },
        { silent: true },
      ).catch((cause: unknown) => {
        notify("保存界面偏好失败", "error", errorMessage(cause));
      });
    },
    [notify, saveSettings, settings],
  );

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

  // 全库特征覆盖情况只随数据变化重查；用于筛选面板与工具栏置灰。
  useEffect(() => {
    let disposed = false;
    void libraryApi
      .getFeatureSummary()
      .then((summary) => {
        if (!disposed) setFeatureSummary(summary);
      })
      .catch(() => {
        // 统计失败时不置灰，避免把暂时读不到当成“没有特征”。
      });
    return () => {
      disposed = true;
    };
  }, [refreshToken, dataVersion]);

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

  // 新建集合对话框里要显示“当前视图 N 张”，打开时取一次总数（不拉全量 id）。
  useEffect(() => {
    if (!createCollectionOpen) return;
    let cancelled = false;
    setViewCount(null);
    void libraryApi
      .getFilterCounts(query)
      .then((counts) => {
        if (!cancelled) setViewCount(counts.total);
      })
      .catch(() => {
        if (!cancelled) setViewCount(null);
      });
    return () => {
      cancelled = true;
    };
  }, [createCollectionOpen, query]);

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

  // 单击只选中；双击 / Enter 打开查看器；详情面板由按钮或右键控制，避免每次点图都挤动网格。
  const handleItemClick = useCallback(
    (item: ImageRow, event: ReactMouseEvent) => {
      setContextMenu(null);
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
      setContextMenu({
        x: event.clientX,
        y: event.clientY,
        items: targetItems.length > 0 ? targetItems : [item],
        ids: ids.length > 0 ? ids : [item.id],
      });
      // 菜单先出现，能力随后补齐；只在目标仍是同一张图时写回。
      setContextCaps(null);
      contextCapsTarget.current = item.id;
      void libraryApi
        .getImageCapabilities([item.id])
        .then((caps) => {
          if (contextCapsTarget.current === item.id) {
            setContextCaps(caps[0] ?? null);
          }
        })
        .catch(() => {
          // 查询失败时按“未知”处理，菜单项保持可用。
        });
    },
    [items, selected],
  );

  const handleBackgroundClick = useCallback(() => {
    setContextMenu(null);
    clearSelection();
  }, [clearSelection]);

  /** 打开批量任务前先算好各范围的数量；id 集合仍由 Rust 侧解析。 */
  const openBatchTasks = useCallback(async () => {
    setTaskMenuOpen(false);
    if (!onOpenBatchTasks) return;
    const folderId =
      browse.filters.folderIds.length === 1 ? browse.filters.folderIds[0] : null;
    const folder = folderId != null ? folders.find((item) => item.id === folderId) : undefined;
    const collection =
      effectiveView === "collection"
        ? collections.find((item) => item.id === effectiveCollectionId)
        : undefined;
    let viewTotal: number | null = null;
    if (!similarityActive) {
      try {
        viewTotal = (await libraryApi.getFilterCounts(query)).total;
      } catch {
        viewTotal = null;
      }
    }
    onOpenBatchTasks({
      selectedIds: [...selected],
      query: similarityActive ? null : query,
      viewTotal,
      folderId,
      folderName: folder?.path ?? null,
      folderTotal: folder?.totalImageCount ?? null,
      collectionId: effectiveView === "collection" ? effectiveCollectionId : null,
      collectionName: collection?.name ?? null,
      collectionTotal: collection?.imageCount ?? null,
      libraryTotal: current?.imageCount ?? 0,
    });
  }, [
    browse.filters.folderIds,
    collections,
    current?.imageCount,
    effectiveCollectionId,
    effectiveView,
    folders,
    onOpenBatchTasks,
    query,
    selected,
    similarityActive,
  ]);

  const resolveResultIds = useCallback(async () => {
    if (similarityActive && similarity) return similarity.ids();
    return libraryApi.queryImageIds(query);
  }, [query, similarity, similarityActive]);

  /** 复制一个已有集合的内容：按集合顺序取全部 id，不受当前筛选影响。 */
  const resolveCollectionIds = useCallback(
    async (collectionId: number) =>
      libraryApi.queryImageIds({
        view: "collection",
        collectionId,
        search: "",
        searchScope: "view",
        filters: emptyFilters(),
        sort: COLLECTION_SORT,
        cursor: null,
        limit: null,
      }),
    [],
  );

  const selectAllMatches = useCallback(async () => {
    if (selectingAll) return;
    setSelectingAll(true);
    try {
      const ids = await resolveResultIds();
      setSelected(new Set(ids));
      const first = ids[0] ?? null;
      setAnchorId(first);
      setActiveId(first);
      notify(`已选择 ${ids.length.toLocaleString()} 张`, "info");
    } catch (cause) {
      notify("全选失败", "error", errorMessage(cause));
    } finally {
      setSelectingAll(false);
    }
  }, [notify, resolveResultIds, selectingAll]);

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
          ? `另有 ${hidden.toLocaleString()} 张未复制`
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
      if (!(await confirmTrash(confirm, ids.length))) return false;
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
    [confirm, notify, refreshData, viewBusy],
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
      const accepted = await confirmDelete(confirm, ids.length);
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
    const accepted = await confirmEmptyTrash(confirm, count);
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

  const handleCollectionCreated = useCallback(
    (collection: CollectionInfo) => {
      setCreateCollectionOpen(false);
      setCreatePreset(null);
      clearSelection();
      void reloadCollections();
      applyBrowse({
        ...browseRef.current,
        view: "collection",
        collectionId: collection.id,
        searchScope: "view",
        sort: COLLECTION_SORT,
      });
    },
    [applyBrowse, clearSelection, reloadCollections],
  );

  const deleteCollection = useCallback(
    async (collection: CollectionInfo) => {
      const accepted = await confirm({
        title: `删除集合「${collection.name}」？`,
        message: `集合内的 ${collection.imageCount.toLocaleString()} 张图片不会被删除，只会移除集合本身。`,
        confirmLabel: "删除集合",
        danger: true,
      });
      if (!accepted) return;
      try {
        await libraryApi.deleteCollection(collection.id);
        notify("集合已删除", "success");
        if (browseRef.current.collectionId === collection.id) {
          applyBrowse({
            ...browseRef.current,
            view: "all",
            collectionId: null,
            sort: sortForView("all", browseRef.current.sort),
          });
        }
        void reloadCollections();
      } catch (cause) {
        notify("删除集合失败", "error", errorMessage(cause));
      }
    },
    [applyBrowse, confirm, notify, reloadCollections],
  );

  const moveCollection = useCallback(
    async (collection: CollectionInfo, delta: number) => {
      const index = collections.findIndex((item) => item.id === collection.id);
      const target = index + delta;
      if (index < 0 || target < 0 || target >= collections.length) return;
      const next = [...collections];
      [next[index], next[target]] = [next[target], next[index]];
      try {
        await libraryApi.reorderCollections(next.map((item) => item.id));
        void reloadCollections();
      } catch (cause) {
        notify("调整集合顺序失败", "error", errorMessage(cause));
      }
    },
    [collections, notify, reloadCollections],
  );

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
      if (taskMenuOpen) setTaskMenuOpen(false);
      else if (filterOpen) setFilterOpen(false);
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
      <div className="library-topbar">
        <IconButton label="返回上一视图" disabled={!canGoBack} onClick={goBack}>
          <ArrowLeft size={14} />
        </IconButton>
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
          onNewCollection={() => {
            setCreatePreset(null);
            setCreateCollectionOpen(true);
          }}
          onManageCollections={() => setCollectionManagerOpen(true)}
          onCombineCollections={() => setCombineOpen(true)}
          onRenameCollection={setRenameTarget}
          onDeleteCollection={(collection) => void deleteCollection(collection)}
          onMoveCollection={(collection, delta) => void moveCollection(collection, delta)}
          onDuplicateCollection={(collection) => {
            setCreatePreset(collection);
            setCreateCollectionOpen(true);
          }}
          onEmptyTrash={() => void emptyTrash()}
        />

        <div className="library-topbar-actions">
          {selected.size > 0 ? (
            <div className="selection-summary">
              {selectingAll ? <Spinner size={13} /> : null}
              <span>已选 {selected.size.toLocaleString()} 张</span>
              <IconButton label="清除选择" onClick={clearSelection}>
                <X size={13} />
              </IconButton>
            </div>
          ) : null}
          <Button
            size="sm"
            variant="primary"
            icon={<Upload size={14} />}
            disabled={!onOpenImport}
            onClick={() => onOpenImport?.()}
          >
            导入
          </Button>
          <button
            ref={taskAnchorRef}
            type="button"
            className="button button-secondary button-sm"
            aria-expanded={taskMenuOpen}
            onClick={() => setTaskMenuOpen((open) => !open)}
          >
            <Wrench size={14} />
            任务
          </button>
          <Popover
            open={taskMenuOpen}
            anchorRef={taskAnchorRef}
            placement="bottom-end"
            onClose={() => setTaskMenuOpen(false)}
            minWidth={190}
          >
            <MenuSection title="工具">
              <button
                type="button"
                className="context-menu-item"
                disabled={!onOpenBatchTasks}
                onClick={() => void openBatchTasks()}
              >
                <Sparkles size={14} />
                <span>批量任务…</span>
              </button>
              <button
                type="button"
                className="context-menu-item"
                disabled={!onOpenSimilarityGroups}
                onClick={() => {
                  setTaskMenuOpen(false);
                  onOpenSimilarityGroups?.();
                }}
              >
                <ScanSearch size={14} />
                <span>相似分组…</span>
              </button>
            </MenuSection>
          </Popover>
        </div>
      </div>

      <div className="library-toolbar">
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

        <div className="library-toolbar-right">
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
            disabled={!similarity || Boolean(semanticBlockedReason)}
            title={!similarity ? "当前不可用" : (semanticBlockedReason ?? undefined)}
            onClick={() => setSemanticOpen((open) => !open)}
          >
            语义
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
              featureSummary={featureSummary}
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

          <div className="segmented" role="group" aria-label="缩略图填充">
            {THUMB_FITS.map(({ value, label, icon: Icon }) => (
              <button
                key={value}
                type="button"
                className={thumbFit === value ? "is-active" : ""}
                title={
                  viewMode === "list"
                    ? `${label}（列表视图不裁剪缩略图）`
                    : `${label}：${value === "cover" ? "裁掉超出方格的部分" : "长图也能完整看到"}`
                }
                aria-label={label}
                disabled={viewMode === "list"}
                onClick={() => persistUi({ thumbFit: value })}
              >
                <Icon size={15} />
              </button>
            ))}
          </div>

          <IconButton
            label={inspectorOpen ? "隐藏详情侧栏" : "显示详情侧栏"}
            className={inspectorOpen ? "is-active" : ""}
            onClick={() => setInspectorOpen((value) => !value)}
          >
            <PanelRight size={14} />
          </IconButton>

          <IconButton label="重新读取当前视图" onClick={retry}>
            <RefreshCw size={14} />
          </IconButton>
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
              (similarity.busy ? "正在理解描述…" : "描述只用于这次搜索，不会写入图库。")}
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
          </span>
          <Button
            size="sm"
            variant="secondary"
            icon={<X size={12} />}
            onClick={() => similarity.clear()}
          >
            退出相似结果
          </Button>
        </div>
      ) : null}

      {filterSummary.length > 0 && !similarityActive ? (
        <div className="filter-summary">
          <span>{filterSummary.join(" · ")}</span>
          {hasActiveFilters(browse.filters) ? (
            <Button size="sm" variant="ghost" onClick={resetFilters}>
              清除筛选
            </Button>
          ) : null}
          {browse.search ? (
            <Button size="sm" variant="ghost" onClick={() => setSearchDraft("")}>
              清除搜索
            </Button>
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
                    : "正在理解描述"
                  : "正在读取图片"}
              </strong>
              <span>
                {similarityActive ? "正在搜索…" : "正在读取…"}
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
                    ? "这张图片还没有可比较的特征，或者库里没有足够相似的图片。"
                    : "换一种说法再试。"
                  : hasActiveQuery(browse)
                    ? "调整搜索或筛选条件，或者清除后查看全部图片。"
                    : effectiveView === "trash"
                      ? "从图库移入回收站的图片会显示在这里，可以恢复或永久删除。"
                      : effectiveView === "collection"
                        ? "把选中的图片加入这个集合后，它们会出现在这里。"
                        : "导入图片后会显示在这里。"}
              </span>
              {similarityActive ? null : hasActiveFilters(browse.filters) ? (
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
              thumbFit={thumbFit}
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

        {inspectorOpen ? (
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
          capabilities={contextCaps}
          inCollection={effectiveView === "collection"}
          hasActive={contextHasActive}
          hasDeleted={contextHasDeleted}
          onClose={() => setContextMenu(null)}
          onOpenViewer={() => {
            const item = contextMenu.items[0];
            setContextMenu(null);
            if (item) activateItem(item);
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

      <CreateCollectionDialog
        open={createCollectionOpen}
        collections={collections}
        selectionIds={selectedIds}
        viewLabel={similarityActive ? "相似结果" : "当前视图"}
        viewCount={viewCount}
        resolveViewIds={resolveResultIds}
        presetCollection={createPreset}
        resolveCollectionIds={resolveCollectionIds}
        onCreated={handleCollectionCreated}
        onClose={() => {
          setCreateCollectionOpen(false);
          setCreatePreset(null);
        }}
      />
      <RenameCollectionDialog
        collection={renameTarget}
        onRenamed={() => {
          setRenameTarget(null);
          notify("集合已重命名", "success");
          void reloadCollections();
        }}
        onClose={() => setRenameTarget(null)}
      />
      <CombineCollectionsDialog
        open={combineOpen}
        collections={collections}
        onCombined={() => {
          void reloadCollections();
          refreshData();
        }}
        onClose={() => setCombineOpen(false)}
      />
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
