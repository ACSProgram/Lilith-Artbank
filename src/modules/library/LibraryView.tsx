import {
  startTransition,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";
import {
  AlertTriangle,
  ArrowLeft,
  ChevronRight,
  Crop,
  FilterX,
  FolderOpen,
  HardDrive,
  History,
  ImageOff,
  LayoutGrid,
  List,
  Maximize,
  PanelRight,
  RefreshCw,
  ScanSearch,
  Search,
  ShieldCheck,
  SlidersHorizontal,
  Sparkles,
  Upload,
  Wrench,
  X,
} from "lucide-react";
import { useAppRuntime } from "../../app/runtimeContext";
import { useNotices } from "../../app/noticeContext";
import { useTaskActive } from "../../app/taskContext";
import { Button, IconButton } from "../../shared/components/Button";
import { MenuSection } from "../../shared/components/MenuSection";
import { Popover } from "../../shared/components/Popover";
import { Spinner } from "../../shared/components/Spinner";
import { copyText } from "../../shared/clipboard";
import { confirmDelete, confirmEmptyTrash, confirmTrash } from "../../shared/confirm";
import { errorMessage } from "../../shared/errors";
import { LIST_COLUMNS, type ThumbFit, type UiSettings } from "../../app/types";
import { libraryApi } from "./api";
import { CollectionManagerDialog } from "./CollectionManagerDialog";
import { CollectionPickerDialog } from "./CollectionPickerDialog";
import {
  CombineCollectionsDialog,
  CreateCollectionDialog,
  RenameCollectionDialog,
} from "./CollectionDialogs";
import { FilterPanel } from "./FilterPanel";
import { FindSimilarPanel, type SimilarReference } from "./FindSimilarPanel";
import { InspectorPanel } from "./Inspector";
import { useStats, useStatsStore } from "./StatsProvider";
import { LibraryContextMenu, type ContextMenuState } from "./LibraryContextMenu";
import { OperationHistoryPanel } from "./OperationHistoryPanel";
import { SortMenu } from "./SortMenu";
import { TagManagerDialog, type TagChange } from "./TagManagerDialog";
import { ViewBar } from "./ViewBar";
import { breadcrumbsFor, buildFolderTree } from "./folderTreeModel";
import {
  COLLECTION_SORT_LEVELS,
  activeFilterCount,
  browseToFolder,
  buildQuery,
  effectiveViewOf,
  emptyFilters,
  folderLabel,
  hasActiveFilters,
  hasActiveQuery,
  isFormTarget,
  queryIdentityKey,
  queryKey,
  rowInTagScope,
  selectionRange,
  sortForViewChange,
  tagFilterState,
  type BrowseState,
  type SimilarityBrowseState,
} from "./model";
import type { BatchTaskContextInfo } from "../../shared/batchTaskContext";
import type {
  CollectionInfo,
  FeatureSummary,
  FolderInfo,
  ImageDetails,
  ImageRow,
  LibraryView as LibraryViewType,
  SortKey,
  TrashFolders,
  ViewMode,
} from "./types";
import { useImageQuery } from "./useImageQuery";
import { VirtualGrid } from "./VirtualGrid";
import { VirtualList } from "./VirtualList";
import type { VirtualViewHandle } from "./virtual";
import {
  combinePlans,
  planDataChange,
  selectionAfterChange,
  sumCountDelta,
  touchesTags,
  type DataChange,
  type DataChangeSignal,
} from "./dataChange";
import type {
  ViewerSession,
  ViewerSessionItem,
  ViewerSessionUpdate,
} from "../../shared/viewerSession";
import type { TagFilterAction, TagFilterScope } from "../../shared/tagFilter";
import {
  similarKindLabel,
  type FindSimilarConfig,
} from "../../shared/similarity";

interface LibraryViewProps {
  browse: BrowseState;
  canGoBack: boolean;
  folders: FolderInfo[];
  /** 回收站视图左侧树的文件夹；只用于面包屑与清空按钮。 */
  trashFolders?: TrashFolders | null;
  onApplyBrowse: (next: BrowseState, pushHistory?: boolean) => void;
  onGoBack: () => void;
  onFoldersChanged?: () => void | Promise<void>;
  onImageSelectionChange?: (imageIds: number[]) => void;
  /** 工作区、任务与 MCP 送来的数据变化；本地变化由 LibraryView 直接应用。 */
  changeSignal: DataChangeSignal | null;
  onOpenViewer: (session: ViewerSession) => void;
  /** 结果集或相似来源变化时关闭查看器，避免停在旧数据源上。 */
  onCloseViewer?: () => void;
  /** 计数类状态变化：打开着的任务/找相似对话框需要重取范围数量。 */
  onContextsChanged?: () => void;
  onCopyImage: (imageId: number) => void;
  onOpenImport?: () => void;
  /** 开始把网格/列表中的图片拖到文件夹。 */
  onBeginImageDrag: (imageIds: number[], label: string, event: ReactPointerEvent<HTMLDivElement>) => void;
  similarity?: SimilarityBrowseState;
  onOpenSimilarityGroups?: (context: BatchTaskContextInfo) => void;
  onOpenBatchTasks?: (context: BatchTaskContextInfo) => void;
  onOpenOptimize?: (context: BatchTaskContextInfo) => void;
  /** 打开维护与检查；与批量任务共用同一份范围上下文。 */
  onOpenMaintenance?: (context: BatchTaskContextInfo) => void;
  onGenerateDescription?: (imageId: number) => Promise<{ text: string; model: string }>;
  /** 语义搜索不可用的原因；为空表示可用。 */
  semanticBlockedReason?: string | null;
}

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

type SearchMode = "match" | "semantic";

function initialSearchMode(): SearchMode {
  try {
    return localStorage.getItem("lilith.searchMode") === "semantic"
      ? "semantic"
      : "match";
  } catch {
    return "match";
  }
}

function fileNameOf(path: string): string {
  const index = Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/"));
  return index >= 0 ? path.slice(index + 1) : path;
}

/**
 * 活动图是否已离开结果集：已选中的按选择裁剪结果判断，未选中的只认明确移出图库的成员变化。
 * 评分/标签/描述这类不改变范围的变化不会移动活动图。
 */
function activeLeftResult(
  active: number,
  previousSelected: Set<number>,
  nextSelected: Set<number>,
  changes: DataChange[],
): boolean {
  if (previousSelected.has(active)) return !nextSelected.has(active);
  return changes.some(
    (change) =>
      change.kind === "membership" &&
      !isRestoreReason(change.reason) &&
      change.ids?.includes(active) === true,
  );
}

/** 恢复类变化让图片重新出现，不按“移出图库”处理。 */
function isRestoreReason(reason: string | undefined): boolean {
  return reason === "restore" || reason === "images_restore";
}

export function LibraryView({
  browse,
  canGoBack,
  folders,
  trashFolders = null,
  onApplyBrowse,
  onGoBack,
  onFoldersChanged,
  onImageSelectionChange,
  changeSignal,
  onOpenViewer,
  onCloseViewer,
  onContextsChanged,
  onCopyImage,
  onOpenImport,
  onBeginImageDrag,
  similarity,
  onOpenSimilarityGroups,
  onOpenBatchTasks,
  onOpenOptimize,
  onOpenMaintenance,
  onGenerateDescription,
  semanticBlockedReason = null,
}: LibraryViewProps) {
  const { settings, current, refreshStatus, adjustCounts, saveSettings } = useAppRuntime();
  const { notify, confirm } = useNotices();
  const { count: activeTaskCount, label: activeTaskLabel } = useTaskActive();

  const [searchDraft, setSearchDraft] = useState(browse.search);
  const [searchMode, setSearchMode] = useState<SearchMode>(initialSearchMode);
  // 记住的是用户偏好；AI 离线或全库没有 embedding 时退回匹配模式，
  // 否则重启后会停在一个永远禁用的搜索框里。
  const semanticMode = searchMode === "semantic" && !semanticBlockedReason;
  const [viewMode, setViewMode] = useState<ViewMode>(initialViewMode);
  const [filterOpen, setFilterOpen] = useState(false);
  const [inspectorOpen, setInspectorOpen] = useState(true);
  const [selected, setSelected] = useState<Set<number>>(() => new Set());
  const [activeId, setActiveId] = useState<number | null>(null);
  const [anchorId, setAnchorId] = useState<number | null>(null);
  const [gridColumns, setGridColumns] = useState(1);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [selectingAll, setSelectingAll] = useState(false);
  const [detailVersion, setDetailVersion] = useState(0);
  const [tagVersion, setTagVersion] = useState(0);
  const [tagOptionsVersion, setTagOptionsVersion] = useState(0);
  const [collections, setCollections] = useState<CollectionInfo[]>([]);
  const [collectionsLoaded, setCollectionsLoaded] = useState(false);
  const [collectionManagerOpen, setCollectionManagerOpen] = useState(false);
  const [collectionPickerOpen, setCollectionPickerOpen] = useState(false);
  const [createCollectionOpen, setCreateCollectionOpen] = useState(false);
  const [createPreset, setCreatePreset] = useState<CollectionInfo | null>(null);
  const [renameTarget, setRenameTarget] = useState<CollectionInfo | null>(null);
  const [combineOpen, setCombineOpen] = useState(false);
  const [taskMenuOpen, setTaskMenuOpen] = useState(false);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [tagManagerOpen, setTagManagerOpen] = useState(false);
  const [emptyingTrash, setEmptyingTrash] = useState(false);
  const [viewBusy, setViewBusy] = useState(false);
  const [semanticDraft, setSemanticDraft] = useState("");
  const [similarityTarget, setSimilarityTarget] = useState<ImageDetails | null>(null);
  const [similarityReference, setSimilarityReference] =
    useState<SimilarReference | null>(null);
  const [findSimilarOpen, setFindSimilarOpen] = useState(false);
  const [findSimilarContext, setFindSimilarContext] =
    useState<BatchTaskContextInfo | null>(null);
  const [findSimilarInitial, setFindSimilarInitial] =
    useState<FindSimilarConfig | null>(null);
  const [lastFindSimilar, setLastFindSimilar] =
    useState<FindSimilarConfig | null>(null);
  const [featureSummary, setFeatureSummary] = useState<FeatureSummary | null>(null);
  /** 数据变化时递增；摘要读过的版本落后就视为过期。 */
  const [featureEpoch, setFeatureEpoch] = useState(0);
  const [featureSummaryEpoch, setFeatureSummaryEpoch] = useState(-1);
  const [featureSummaryLoading, setFeatureSummaryLoading] = useState(false);
  const changeMarkSeq = useRef(0);

  const viewRef = useRef<VirtualViewHandle>(null);
  const filterAnchorRef = useRef<HTMLDivElement>(null);
  const findSimilarAnchorRef = useRef<HTMLDivElement>(null);
  const taskAnchorRef = useRef<HTMLButtonElement>(null);
  const historyAnchorRef = useRef<HTMLButtonElement>(null);
  const searchInputRef = useRef<HTMLInputElement>(null);
  const browseRef = useRef(browse);
  browseRef.current = browse;
  const activeIdRef = useRef<number | null>(activeId);
  activeIdRef.current = activeId;
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  const appliedChangeRevision = useRef(0);
  const [findSimilarContextVersion, setFindSimilarContextVersion] = useState(0);
  // 标签筛选后保留原图选择；若不在第一页，用锚点游标跳到它所在位置。
  const preserveTagSelectionRef = useRef(false);
  const pendingTagFocusRef = useRef<{
    id: number;
    row: ImageRow;
    jumpTried: boolean;
    queryKey: string;
  } | null>(null);
  const applyBrowse = onApplyBrowse;

  const folderTree = useMemo(
    () =>
      buildFolderTree(
        browse.view === "trash" ? (trashFolders?.folders ?? []) : folders,
      ),
    [browse.view, folders, trashFolders],
  );
  const breadcrumbs = useMemo(() => {
    const trail = breadcrumbsFor(folderTree, browse.filters.folderIds);
    if (browse.view === "trash" && trail.length > 0) {
      trail[0] = { ...trail[0], label: "回收站" };
    }
    return trail;
  }, [folderTree, browse.filters.folderIds, browse.view]);

  const effectiveView: LibraryViewType =
    browse.search && browse.searchScope === "library" ? "all" : browse.view;
  const effectiveCollectionId =
    effectiveView === "collection" ? browse.collectionId : null;

  // 标签筛选范围菜单里的当前文件夹/当前集合名称；没有对应范围时菜单项置灰。
  const folderScopeLabel = useMemo(() => {
    const selected = folders.filter((folder) =>
      browse.filters.folderIds.includes(folder.id),
    );
    if (selected.length === 0) return null;
    const names = selected.map((folder) => folderLabel(folder.path));
    return names.length === 1 ? names[0] : `${names[0]} +${names.length - 1}`;
  }, [browse.filters.folderIds, folders]);
  const collectionScopeLabel = useMemo(() => {
    if (effectiveCollectionId == null) return null;
    return collections.find((collection) => collection.id === effectiveCollectionId)?.name ?? null;
  }, [collections, effectiveCollectionId]);

  const query = useMemo(() => buildQuery(browse), [browse]);
  const imageQuery = useImageQuery(query);
  const stats = useStatsStore();
  const viewCountStats = useStats("summary", query, createCollectionOpen);
  const viewCount = viewCountStats.data?.total ?? null;
  const similarityActive = similarity?.active === true;
  const folderStatsContext = useMemo(
    () =>
      !similarityActive && (query.view === "all" || query.view === "collection")
        ? { query }
        : null,
    [similarityActive, query],
  );
  // 结果来自“找相似”而不是文本语义搜索；状态文案按它分支。
  const similarMode = similarity?.mode === "similar";
  // 相似模式下网格换成结果数据源；两者共享 items/status/error/hasMore 契约。
  const listSource = similarityActive && similarity ? similarity : imageQuery;
  const { items, status, error, moreStatus, moreError, hasMore } = listSource;
  const refreshError = imageQuery.refreshError;
  const clearRefreshError = imageQuery.clearRefreshError;
  // 取稳定回调而不是整个控制器对象，避免每次渲染都让变化应用函数换新引用。
  const { patchItems, removeItems, refreshKeepingRows } = imageQuery;
  const loadMore = listSource.loadMore;
  const retry = listSource.retry;
  const retryMore = similarityActive && similarity ? similarity.loadMore : imageQuery.retryMore;
  const similarityKey = similarityActive ? `similarity:${similarity?.token ?? ""}` : "";
  const similarityClear = similarity?.clear;
  // 排序与数据刷新不再改变 resetKey；只有换结果集或换相似来源才回到顶部。
  const resetSignature = useMemo(
    () => `${queryIdentityKey(query)}|${similarityKey}`,
    [query, similarityKey],
  );
  const thumbPx = settings?.ui.gridThumbPx ?? 220;
  const thumbFit: ThumbFit = settings?.ui.thumbFit ?? "cover";
  const listColumns = settings?.ui.listColumns ?? LIST_COLUMNS;

  const itemsRef = useRef(items);
  itemsRef.current = items;
  const hasMoreRef = useRef(hasMore);
  hasMoreRef.current = hasMore;
  const viewerListeners = useRef(new Set<() => void>());
  const targetViewerItems = useRef<ViewerSessionItem[]>([]);

  useEffect(() => {
    for (const listener of viewerListeners.current) listener();
  }, [items]);

  // 相似结果的目标图不在结果里，单独取一次详情用于标题与“打开比较图”。
  // 平均参考只取第一张用于打开查看器，标题仍然显示参考图数量。
  const similarityTargetId = similarMode ? (similarity?.targetIds[0] ?? null) : null;
  useEffect(() => {
    if (similarityTargetId == null) {
      setSimilarityTarget(null);
      return;
    }
    let disposed = false;
    void libraryApi
      .getImageDetail(similarityTargetId)
      .then((details) => {
        if (disposed) return;
        if (details.deletedAt != null) {
          setSimilarityTarget(null);
          similarityClear?.();
          notify(
            "比较图已失效",
            "info",
            "目标图片已被移出图库，请重新查找相似图片。",
          );
          return;
        }
        setSimilarityTarget(details);
      })
      .catch(() => {
        if (disposed) return;
        setSimilarityTarget(null);
        similarityClear?.();
        notify(
          "比较图已失效",
          "info",
          "目标图片已被删除，请重新查找相似图片。",
        );
      });
    return () => {
      disposed = true;
    };
  }, [similarityTargetId, detailVersion, similarityClear, notify]);

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

  /** 成员变化后复核相似参考与比较图；本地已知 id 直接剔除，外部变化查一次状态。 */
  const prunedInvalidSimilarityInputs = useCallback(
    async (changes: DataChange[]) => {
      const membership = changes.filter(
        (change) => change.kind === "membership" || change.kind === "unknown",
      );
      if (membership.length === 0) return;

      const removed = new Set<number>();
      let needsQuery = false;
      for (const change of membership) {
        if (!change.ids?.length) {
          needsQuery = true;
          continue;
        }
        // 恢复是图片重新出现，不会让参考或比较图失效。
        if (isRestoreReason(change.reason)) continue;
        for (const id of change.ids) removed.add(id);
      }

      const referenceIds = similarityReference?.ids ?? [];
      const targetIds = lastFindSimilar?.targetIds ?? similarity?.targetIds ?? [];
      const checkIds = [...new Set([...referenceIds, ...targetIds])];
      const states = new Map<number, number | null>();
      if (needsQuery && checkIds.length > 0) {
        try {
          const rows = await libraryApi.getImageStates(checkIds);
          for (const row of rows) states.set(row.id, row.deletedAt);
        } catch {
          return;
        }
      }
      const isValid = (id: number) =>
        !removed.has(id) &&
        (!needsQuery || (states.has(id) && states.get(id) == null));
      const remainingReferences = referenceIds.filter(isValid);
      const remainingTargets = targetIds.filter(isValid);
      let cleared = false;
      const clearResults = () => {
        if (cleared) return;
        cleared = true;
        similarityClear?.();
        setSimilarityTarget(null);
      };

      if (referenceIds.length > 0 && remainingReferences.length !== referenceIds.length) {
        if (remainingReferences.length === 0) {
          setSimilarityReference(null);
          setLastFindSimilar(null);
          setFindSimilarOpen(false);
          clearResults();
          notify(
            "相似参考已失效",
            "info",
            "参考图已被移出图库，请重新选择参考图。",
          );
        } else {
          const remainingSet = new Set(remainingReferences);
          setSimilarityReference({
            ids: remainingReferences,
            items:
              similarityReference?.items.filter((item) => remainingSet.has(item.id)) ?? [],
          });
          if (lastFindSimilar?.targetIds.some((id) => !remainingSet.has(id))) {
            setLastFindSimilar(null);
            clearResults();
            notify(
              "相似参考已变化",
              "info",
              "有参考图被移出图库，当前相似结果已失效。",
            );
          }
        }
      }

      if (targetIds.length > 0 && remainingTargets.length !== targetIds.length) {
        if (lastFindSimilar?.targetIds.some((id) => !remainingTargets.includes(id))) {
          setLastFindSimilar(null);
        }
        clearResults();
        notify(
          "比较图已失效",
          "info",
          "目标图片已被移出图库，请重新查找相似图片。",
        );
      }
    },
    [
      lastFindSimilar,
      notify,
      similarity,
      similarityClear,
      similarityReference,
    ],
  );

  /**
   * 统一应用数据变化。列表刷新保留旧行；只有换结果集才清空。
   * 文件夹/集合/状态/特征/分面按 plan 各自刷新，不再让所有面板订阅同一个版本号。
   * 列表刷新是唯一必须等待的步骤；文件夹树与计数在后台过渡更新，先恢复交互。
   */
  const applyChanges = useCallback(
    async (changes: DataChange[]) => {
      const meaningful = changes.filter((change) => change.kind !== "none");
      if (meaningful.length === 0) return;

      const markId = `library-change-${(changeMarkSeq.current += 1)}`;
      performance.mark(`${markId}:start`);
      const stageTimes: string[] = [];
      const measureStage = async <T,>(name: string, work: Promise<T>): Promise<T> => {
        const start = `${markId}:${name}:start`;
        const end = `${markId}:${name}:end`;
        performance.mark(start);
        try {
          return await work;
        } finally {
          performance.mark(end);
          const measure = performance.measure(`${markId}:${name}`, start, end);
          stageTimes.push(`${name} ${measure.duration.toFixed(0)}ms`);
          performance.clearMarks(start);
          performance.clearMarks(end);
          performance.clearMeasures(`${markId}:${name}`);
        }
      };

      const plans = meaningful.map((change) =>
        planDataChange(change, query, { hasMore: hasMoreRef.current }),
      );
      const plan = combinePlans(plans);
      // 删除/合并掉的标签还留在筛选条件里，列表会永远为空且用户无从取消。
      // 清掉它等于换了一次查询身份，列表由 loadFirst 重取，这一轮不再做保留旧行的刷新。
      const staleFilterTags = plan.dropTags.filter(
        (tagId) =>
          query.filters.tagIds.includes(tagId) || query.filters.excludeTagIds.includes(tagId),
      );
      if (staleFilterTags.length > 0) {
        const stale = new Set(staleFilterTags);
        const currentBrowse = browseRef.current;
        onApplyBrowse(
          {
            ...currentBrowse,
            filters: {
              ...currentBrowse.filters,
              tagIds: currentBrowse.filters.tagIds.filter((id) => !stale.has(id)),
              excludeTagIds: currentBrowse.filters.excludeTagIds.filter((id) => !stale.has(id)),
            },
          },
          false,
        );
      }

      // 同一个 id 只留一份合并后的补丁；评分/文件夹这类批量写入按 id + 新值推导。
      const patchById = new Map<number, Partial<ImageRow>>();
      const addPatch = (id: number, update: Partial<ImageRow>) => {
        patchById.set(id, { ...patchById.get(id), ...update });
      };
      for (const change of meaningful) {
        for (const patch of change.rowPatches ?? []) addPatch(patch.id, patch.update);
        if (change.ids?.length && (change.fields ?? []).includes("score")) {
          for (const id of change.ids) addPatch(id, { score: change.score ?? 0 });
        }
        if (change.kind === "folder" && change.ids?.length && change.targetFolderPath != null) {
          for (const id of change.ids) addPatch(id, { folderPath: change.targetFolderPath });
        }
        if (
          change.kind === "membership" &&
          change.source === "local" &&
          change.ids?.length
        ) {
          removeItems(change.ids);
        }
      }
      if (patchById.size > 0) {
        const patches = [...patchById].map(([id, update]) => ({ id, update }));
        patchItems(patches);
        if (similarityActive && similarity) {
          for (const patch of patches) similarity.patchItem(patch.id, patch.update);
        }
      }

      const folderPaths = folders
        .filter((folder) => query.filters.folderIds.includes(folder.id))
        .map((folder) => folder.path);
      const previousSelected = selectedRef.current;
      let nextSelected = previousSelected;
      for (const change of meaningful) {
        const changeIds = change.ids?.length ? new Set(change.ids) : undefined;
        nextSelected = selectionAfterChange(nextSelected, change, query, folderPaths, changeIds);
      }
      if (
        nextSelected.size !== previousSelected.size ||
        [...nextSelected].some((id) => !previousSelected.has(id))
      ) {
        selectedRef.current = nextSelected;
        setSelected(nextSelected);
      }

      const active = activeIdRef.current;
      if (plan.selection === "clear") {
        setActiveId(null);
        setAnchorId(null);
      } else if (
        active != null &&
        activeLeftResult(active, previousSelected, nextSelected, meaningful)
      ) {
        // 活动图离开结果时把焦点交给剩余选择，其次是已加载的第一张。
        const fallback =
          [...nextSelected][0] ??
          itemsRef.current.find((item) => item.id !== active)?.id ??
          null;
        setActiveId(fallback);
        setAnchorId(fallback);
      }
      setContextMenu(null);

      if (plan.similarity === "clear") {
        similarityClear?.();
        setSimilarityTarget(null);
      }

      // 文件夹树、集合、计数都不阻塞列表刷新；它们放到过渡更新里，先让界面可操作。
      if (plan.folders) {
        startTransition(async () => {
          await onFoldersChanged?.();
        });
      }
      if (plan.collections) {
        startTransition(() => {
          void reloadCollections();
        });
      }
      if (plan.status) {
        const delta = sumCountDelta(meaningful);
        if (delta) {
          adjustCounts(delta);
        } else {
          startTransition(async () => {
            try {
              await refreshStatus();
            } catch (cause) {
              notify("刷新图库状态失败", "error", errorMessage(cause));
            }
          });
        }
      }

      if (plan.list === "refresh" && staleFilterTags.length === 0) {
        await measureStage("list", refreshKeepingRows());
      }

      const touchesTagData = meaningful.some(touchesTags);
      startTransition(() => {
        // 特征摘要只递增版本；筛选面板打开时才真正查询。
        if (plan.features) setFeatureEpoch((value) => value + 1);
        if (plan.facets) stats.invalidate();
        if (plan.tagOptions) setTagOptionsVersion((value) => value + 1);
        if (touchesTagData) setTagVersion((value) => value + 1);
        if (plan.detail) setDetailVersion((value) => value + 1);
        if (plan.folders || plan.collections || plan.status || plan.detail) {
          setFindSimilarContextVersion((value) => value + 1);
        }
      });
      if (plan.facets || plan.folders || plan.collections || plan.status) {
        onContextsChanged?.();
      }

      if (
        meaningful.some(
          (change) =>
            change.kind === "membership" ||
            change.kind === "unknown" ||
            change.kind === "content",
        )
      ) {
        void prunedInvalidSimilarityInputs(meaningful);
      }

      performance.mark(`${markId}:end`);
      const total = performance.measure(markId, `${markId}:start`, `${markId}:end`).duration;
      performance.clearMarks(`${markId}:start`);
      performance.clearMarks(`${markId}:end`);
      performance.clearMeasures(markId);
      if (total >= 120) {
        console.debug(
          `[图库] 数据变化 ${total.toFixed(0)}ms${stageTimes.length > 0 ? `（${stageTimes.join("，")}）` : ""}`,
        );
      }
    },
    [
      adjustCounts,
      folders,
      notify,
      onApplyBrowse,
      onContextsChanged,
      onFoldersChanged,
      patchItems,
      prunedInvalidSimilarityInputs,
      query,
      refreshKeepingRows,
      refreshStatus,
      reloadCollections,
      removeItems,
      similarity,
      similarityActive,
      similarityClear,
      stats,
    ],
  );

  const applyChange = useCallback(
    (change: DataChange) => {
      void applyChanges([change]);
    },
    [applyChanges],
  );

  useEffect(() => {
    if (!changeSignal || changeSignal.revision === appliedChangeRevision.current) return;
    appliedChangeRevision.current = changeSignal.revision;
    void applyChanges(changeSignal.changes);
  }, [changeSignal, applyChanges]);

  // 保留旧行刷新失败时列表仍可用；只提示一次，不替换内容、不打断滚动。
  useEffect(() => {
    if (!refreshError) return;
    notify(
      "刷新图片列表失败",
      "error",
      "当前仍显示旧内容，可以稍后点工具栏的刷新重试。",
    );
    clearRefreshError();
  }, [clearRefreshError, notify, refreshError]);

  useEffect(() => {
    void reloadCollections();
  }, [reloadCollections]);

  // 全库特征覆盖只在筛选面板打开且数据变过时读取；旧值继续显示，不闪空。
  useEffect(() => {
    if (!filterOpen || featureSummaryEpoch === featureEpoch) return;
    const epoch = featureEpoch;
    let disposed = false;
    setFeatureSummaryLoading(true);
    void libraryApi
      .getFeatureSummary()
      .then((summary) => {
        if (disposed) return;
        setFeatureSummary(summary);
        setFeatureSummaryEpoch(epoch);
      })
      .catch(() => {
        // 失败保留旧值，也不把暂时读不到当成“没有特征”；下次打开面板再试。
      })
      .finally(() => {
        if (!disposed) setFeatureSummaryLoading(false);
      });
    return () => {
      disposed = true;
      setFeatureSummaryLoading(false);
    };
  }, [featureEpoch, featureSummaryEpoch, filterOpen]);

  useEffect(() => {
    // 标签筛选是“从当前图片跳转”，保留选择；其它筛选/视图变化仍清空。
    if (preserveTagSelectionRef.current) {
      preserveTagSelectionRef.current = false;
      return;
    }
    setSelected(new Set());
    setActiveId(null);
    setAnchorId(null);
    setContextMenu(null);
  }, [resetSignature]);

  // 底层数据源换了（视图/筛选/相似 token）：查看器里的图片不再属于当前结果。
  const viewerSourceRef = useRef<string | null>(null);
  useEffect(() => {
    if (viewerSourceRef.current == null) {
      viewerSourceRef.current = resetSignature;
      return;
    }
    if (viewerSourceRef.current === resetSignature) return;
    viewerSourceRef.current = resetSignature;
    onCloseViewer?.();
  }, [resetSignature, onCloseViewer]);

  // 标签筛选完成后：回到原图；第一页没有它时，用锚点游标取它所在的一页。
  useEffect(() => {
    const pending = pendingTagFocusRef.current;
    if (!pending || imageQuery.status !== "ready") return;
    // 只认新筛选对应的结果；旧列表可能恰好命中，会误判。
    if (imageQuery.loadedKey !== pending.queryKey) return;
    const index = imageQuery.items.findIndex((item) => item.id === pending.id);
    if (index >= 0) {
      pendingTagFocusRef.current = null;
      setSelected(new Set([pending.id]));
      setActiveId(pending.id);
      setAnchorId(pending.id);
      viewRef.current?.ensureIndex(index);
      return;
    }
    if (!pending.jumpTried && imageQuery.hasMore) {
      pending.jumpTried = true;
      imageQuery.jumpTo(pending.row);
      return;
    }
    // 结果里确实没有这张图：清空选择，避免详情继续显示不在当前结果里的图。
    pendingTagFocusRef.current = null;
    setSelected(new Set());
    setActiveId(null);
    setAnchorId(null);
  }, [
    imageQuery.hasMore,
    imageQuery.items,
    imageQuery.jumpTo,
    imageQuery.loadedKey,
    imageQuery.status,
  ]);

  // 外部刷新（导入、批处理完成、MCP 写入）统一走 changeSignal，由 applyChanges 决定
  // 哪些结果过期；不再把所有变化都当成“相似结果失效”。

  useEffect(() => {
    try {
      localStorage.setItem("lilith.viewMode", viewMode);
    } catch {
      // 忽略无法持久化视图模式的浏览器限制。
    }
  }, [viewMode]);

  useEffect(() => {
    try {
      localStorage.setItem("lilith.searchMode", searchMode);
    } catch {
      // 忽略无法持久化搜索模式的浏览器限制。
    }
  }, [searchMode]);

  useEffect(() => {
    setSearchDraft(browse.search);
  }, [browse.search]);

  useEffect(() => {
    if (semanticMode) return;
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
  }, [searchDraft, semanticMode]);

  useEffect(() => {
    if (!collectionsLoaded || browse.view !== "collection" || browse.collectionId == null) return;
    if (collections.some((collection) => collection.id === browse.collectionId)) return;
    applyBrowse(
      {
        ...browseRef.current,
        ...sortForViewChange(browseRef.current, "all"),
        view: "all",
        collectionId: null,
      },
      false,
    );
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

  useEffect(() => {
    if (!findSimilarOpen) return;
    const onPointerDown = (event: PointerEvent) => {
      if (
        findSimilarAnchorRef.current &&
        !findSimilarAnchorRef.current.contains(event.target as Node)
      ) {
        setFindSimilarOpen(false);
      }
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [findSimilarOpen]);

  const goBack = useCallback(() => {
    onGoBack();
    similarityClear?.();
  }, [onGoBack, similarityClear]);

  // 切换视图保留搜索范围与全局排序；“集合内顺序”只在集合视图里暂存/恢复。
  const showAll = useCallback(() => {
    applyBrowse({
      ...browseRef.current,
      ...sortForViewChange(browseRef.current, "all"),
      view: "all",
      collectionId: null,
      filters: { ...browseRef.current.filters, trashLoose: false },
    });
    similarityClear?.();
  }, [applyBrowse, similarityClear]);

  const showTrash = useCallback(() => {
    applyBrowse({
      ...browseRef.current,
      ...sortForViewChange(browseRef.current, "trash"),
      view: "trash",
      collectionId: null,
      // 左侧会换成回收站树，正常文件夹选择在这里没有落点。
      filters: {
        ...browseRef.current.filters,
        folderIds: [],
        folderDirect: false,
        trashLoose: false,
      },
    });
    similarityClear?.();
  }, [applyBrowse, similarityClear]);

  const showTemp = useCallback(() => {
    applyBrowse({
      ...browseRef.current,
      ...sortForViewChange(browseRef.current, "temp"),
      view: "temp",
      collectionId: null,
      filters: { ...browseRef.current.filters, folderIds: [], trashLoose: false },
    });
    similarityClear?.();
  }, [applyBrowse, similarityClear]);

  const showCollection = useCallback(
    (collectionId: number) => {
      const currentState = browseRef.current;
      applyBrowse({
        ...currentState,
        ...sortForViewChange(currentState, "collection"),
        view: "collection",
        collectionId,
        filters: { ...currentState.filters, trashLoose: false },
      });
      similarityClear?.();
    },
    [applyBrowse, similarityClear],
  );

  const setSearchScope = (scope: BrowseState["searchScope"]) => {
    applyBrowse({ ...browseRef.current, searchScope: scope });
  };

  const switchSearchMode = (mode: SearchMode) => {
    if (mode === searchMode) return;
    if (mode === "semantic") {
      if (!similarity || semanticBlockedReason) return;
      // 语义搜索接管网格：先撤掉“找相似”结果与文字匹配条件，避免三种结果混在一起。
      if (similarityActive) similarityClear?.();
      if (browseRef.current.search) {
        applyBrowse({ ...browseRef.current, search: "", searchScope: "view" }, false);
      }
    } else {
      similarityClear?.();
    }
    setSearchMode(mode);
    searchInputRef.current?.focus();
  };

  const submitSemanticSearch = useCallback(() => {
    const text = semanticDraft.trim();
    if (!text || !similarity || semanticBlockedReason) return;
    similarity.search(text);
  }, [semanticBlockedReason, semanticDraft, similarity]);

  /** 调整排序：立即生效，并把全局排序写回设置。 */
  const applySort = useCallback(
    (nextSort: BrowseState["sort"]) => {
      const state = browseRef.current;
      const hasCollectionOrder = nextSort.some((spec) => spec.key === "collectionPosition");
      applyBrowse(
        {
          ...state,
          sort: nextSort,
          collectionSort: hasCollectionOrder ? nextSort : null,
        },
        false,
      );
      const globalSort = nextSort.filter((spec) => spec.key !== "collectionPosition");
      if (globalSort.length > 0) persistUi({ sort: globalSort });
    },
    [applyBrowse, persistUi],
  );

  /** 列表列头：点击第一级同列切换方向，其它列改为按该列排序。 */
  const handleColumnSort = useCallback(
    (key: SortKey) => {
      const current = browseRef.current.sort;
      if (current.length === 1 && current[0].key === key) {
        applySort([{ key, descending: !current[0].descending }]);
        return;
      }
      applySort([{ key, descending: key !== "folder" && key !== "collectionPosition" }]);
    },
    [applySort],
  );

  useEffect(() => {
    onImageSelectionChange?.([...selected]);
  }, [selected, onImageSelectionChange]);

  const handleItemPointerDown = useCallback(
    (item: ImageRow, event: ReactPointerEvent<HTMLDivElement>) => {
      if (effectiveView === "temp") return;
      const ids = selected.has(item.id) ? [...selected] : [item.id];
      if (ids.length === 0) return;
      onBeginImageDrag(ids, `${ids.length} 张图片`, event);
    },
    [effectiveView, onBeginImageDrag, selected],
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
    },
    [items, selected],
  );

  const handleBackgroundClick = useCallback(() => {
    setContextMenu(null);
    clearSelection();
  }, [clearSelection]);

  /**
   * 打开任务前先算好各范围的数量；id 集合仍由 Rust 侧解析。
   *
   * 相似结果视图里底层图库视图默认不能当范围（批量任务会改错图片），
   * 找相似例外：它要能把结果再收窄到进入相似结果前的那个视图。
   *
   * `viewTotal` 需要一次计数查询，所以单独拆出来：空间优化对话框先用
   * `buildTaskContextBase` 立刻打开，再由工作区异步补这个数。
   */
  const buildTaskContextBase = useCallback(
    (options?: { includeView?: boolean }): BatchTaskContextInfo | null => {
      const includeView = !similarityActive || options?.includeView === true;
      const folderId =
        browse.filters.folderIds.length === 1 ? browse.filters.folderIds[0] : null;
      const folder =
        folderId != null ? folders.find((item) => item.id === folderId) : undefined;
      const collection =
        effectiveView === "collection"
          ? collections.find((item) => item.id === effectiveCollectionId)
          : undefined;
      return {
        selectedIds: [...selected],
        query: includeView ? query : null,
        viewTotal: null,
        folderId,
        folderName: folder?.path ?? null,
        folderDirectTotal: folder?.directImageCount ?? null,
        folderTotal: folder?.totalImageCount ?? null,
        collectionId: effectiveView === "collection" ? effectiveCollectionId : null,
        collectionName: collection?.name ?? null,
        collectionTotal: collection?.imageCount ?? null,
        libraryTotal: current?.imageCount ?? 0,
        tempTotal: current?.tempCount ?? 0,
      };
    },
    [
      browse.filters.folderIds,
      collections,
      current?.imageCount,
      current?.tempCount,
      effectiveCollectionId,
      effectiveView,
      folders,
      query,
      selected,
      similarityActive,
    ],
  );

  const buildTaskContext = useCallback(
    async (options?: { includeView?: boolean }): Promise<BatchTaskContextInfo | null> => {
      const context = buildTaskContextBase(options);
      if (!context?.query) return context;
      let viewTotal: number | null = null;
      try {
        viewTotal = (await stats.fetch("summary", context.query)).total;
      } catch {
        viewTotal = null;
      }
      return { ...context, viewTotal };
    },
    [buildTaskContextBase, stats],
  );

  // 面板打开期间数据变化：只换计数与范围可用性，不清空用户已选的指标与阈值。
  useEffect(() => {
    if (!findSimilarOpen || findSimilarContextVersion === 0) return;
    let disposed = false;
    void buildTaskContext({ includeView: true }).then((context) => {
      if (!disposed && context) setFindSimilarContext(context);
    });
    return () => {
      disposed = true;
    };
  }, [buildTaskContext, findSimilarContextVersion, findSimilarOpen]);

  const openBatchTasks = useCallback(async () => {
    setTaskMenuOpen(false);
    if (!onOpenBatchTasks) return;
    const context = await buildTaskContext();
    if (context) onOpenBatchTasks(context);
  }, [buildTaskContext, onOpenBatchTasks]);

  const openMaintenance = useCallback(async () => {
    setTaskMenuOpen(false);
    if (!onOpenMaintenance) return;
    const context = await buildTaskContext();
    if (context) onOpenMaintenance(context);
  }, [buildTaskContext, onOpenMaintenance]);

  const openOptimize = useCallback(() => {
    setTaskMenuOpen(false);
    if (!onOpenOptimize) return;
    // 先开窗，范围计数由工作区补；不要为了一个数字跑完整分面统计。
    const context = buildTaskContextBase();
    if (context) onOpenOptimize(context);
  }, [buildTaskContextBase, onOpenOptimize]);

  const openSimilarityGroups = useCallback(async () => {
    setTaskMenuOpen(false);
    if (!onOpenSimilarityGroups) return;
    // 相似结果视图下按进入前的图库视图算范围，和找相似面板一致。
    const context = await buildTaskContext({ includeView: true });
    if (context) onOpenSimilarityGroups(context);
  }, [buildTaskContext, onOpenSimilarityGroups]);

  const openFindSimilarPanel = useCallback(
    async (reference: SimilarReference, initial: FindSimilarConfig | null) => {
      const context = await buildTaskContext({ includeView: true });
      if (!context) {
        notify("无法读取当前范围", "error");
        return;
      }
      setFindSimilarContext(context);
      setFindSimilarInitial(initial);
      setFindSimilarOpen(true);
    },
    [buildTaskContext, notify],
  );

  const handleSetSimilarReference = useCallback(
    (ids: number[], items: ImageRow[]) => {
      setContextMenu(null);
      setSimilarityReference({ ids, items });
      setLastFindSimilar(null);
      notify(
        ids.length > 1 ? `已设为 ${ids.length} 张相似参考（平均）` : "已设为相似参考",
        "success",
        "在工具栏点「相似」开始查找。",
      );
    },
    [notify],
  );

  const openFindSimilarFromToolbar = useCallback(() => {
    if (!similarityReference) return;
    void openFindSimilarPanel(similarityReference, lastFindSimilar);
  }, [lastFindSimilar, openFindSimilarPanel, similarityReference]);

  const toggleFindSimilarPanel = useCallback(() => {
    if (findSimilarOpen) {
      setFindSimilarOpen(false);
      return;
    }
    openFindSimilarFromToolbar();
  }, [findSimilarOpen, openFindSimilarFromToolbar]);

  const startFindSimilar = useCallback(
    (config: FindSimilarConfig) => {
      setLastFindSimilar(config);
      setFindSimilarOpen(false);
      similarity?.findSimilar(config);
    },
    [similarity],
  );

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
        sort: COLLECTION_SORT_LEVELS,
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
        applyChange({
          kind: "membership",
          source: "local",
          reason: "trash",
          ids,
          countDelta: { images: -changed, trash: changed },
          similarity: "clear",
        });
        return changed > 0;
      } catch (cause) {
        notify("移入回收站失败", "error", errorMessage(cause));
        return false;
      } finally {
        setViewBusy(false);
      }
    },
    [applyChange, confirm, notify, viewBusy],
  );

  const restoreSelected = useCallback(
    async (ids: number[]): Promise<boolean> => {
      if (ids.length === 0 || viewBusy) return false;
      setViewBusy(true);
      try {
        const changed = await libraryApi.restoreImages(ids);
        notify(`已恢复 ${changed.toLocaleString()} 张`, "success");
        applyChange({
          kind: "membership",
          source: "local",
          reason: "restore",
          ids,
          countDelta: { images: changed, trash: -changed },
          similarity: "clear",
        });
        return changed > 0;
      } catch (cause) {
        notify("恢复失败", "error", errorMessage(cause));
        return false;
      } finally {
        setViewBusy(false);
      }
    },
    [applyChange, notify, viewBusy],
  );

  const deleteSelected = useCallback(
    async (ids: number[]): Promise<boolean> => {
      if (ids.length === 0 || viewBusy) return false;
      const accepted = await confirmDelete(confirm, ids.length);
      if (!accepted) return false;
      setViewBusy(true);
      try {
        const report = await libraryApi.deleteImagesPermanently(ids);
        const failed = report.failures.length;
        notify(
          `已从回收站移除 ${report.deleted.toLocaleString()} 张`,
          failed > 0 ? "error" : "success",
          failed > 0
            ? `${failed} 个文件删除失败：${report.failures[0].reason}`
            : report.deferred && report.deleted > 0
              ? "文件会在操作记录清除后释放。"
              : undefined,
        );
        applyChange({
          kind: "membership",
          source: "local",
          reason: "permanentDelete",
          ids,
          countDelta: { trash: -report.deleted },
          similarity: "clear",
        });
        return report.deleted > 0;
      } catch (cause) {
        notify("永久删除失败", "error", errorMessage(cause));
        return false;
      } finally {
        setViewBusy(false);
      }
    },
    [applyChange, confirm, notify, viewBusy],
  );

  const emptyTrash = useCallback(async () => {
    if (viewBusy) return;
    const count = current?.trashCount ?? 0;
    const folderCount = trashFolders?.folders.length ?? 0;
    const accepted = await confirmEmptyTrash(confirm, count, folderCount);
    if (!accepted) return;
    setEmptyingTrash(true);
    setViewBusy(true);
    try {
      const report = await libraryApi.emptyTrash();
      const cleared = [
        report.deleted > 0 ? `${report.deleted.toLocaleString()} 张图片` : "",
        folderCount > 0 ? `${folderCount.toLocaleString()} 个文件夹` : "",
      ]
        .filter(Boolean)
        .join("、");
      notify(
        `已清空回收站：移出 ${cleared || "0 张"}`,
        report.failures.length > 0 ? "error" : "success",
        report.failures.length > 0
          ? `${report.failures.length} 个文件删除失败：${report.failures[0].reason}`
          : report.deferred && (report.deleted > 0 || folderCount > 0)
            ? "文件会在操作记录清除后释放。"
            : undefined,
      );
      applyChange({
        kind: "membership",
        source: "local",
        reason: "emptyTrash",
        clearSelection: true,
        countDelta: { trash: -report.deleted },
        similarity: "clear",
      });
    } catch (cause) {
      notify("清空回收站失败", "error", errorMessage(cause));
    } finally {
      setEmptyingTrash(false);
      setViewBusy(false);
    }
  }, [applyChange, confirm, current?.trashCount, notify, trashFolders, viewBusy]);

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
        applyChange({
          kind: "collection",
          source: "local",
          reason: "collectionRemove",
          ids,
          similarity: "keep",
        });
        return removed > 0;
      } catch (cause) {
        notify("移出集合失败", "error", errorMessage(cause));
        return false;
      } finally {
        setViewBusy(false);
      }
    },
    [applyChange, effectiveCollectionId, effectiveView, notify, viewBusy],
  );

  /** 编辑保存后只替换已加载行的字段，不重置查询、排序与滚动位置。 */
  const patchSavedImage = useCallback(
    (imageId: number, update: ViewerSessionUpdate) => {
      if (similarityActive && similarity) similarity.patchItem(imageId, update);
      else imageQuery.patchItem(imageId, update);
      applyChange({
        kind: "content",
        source: "local",
        reason: "edit",
        ids: [imageId],
        rowPatches: [{ id: imageId, update }],
        similarity: "clear",
      });
    },
    [applyChange, imageQuery.patchItem, similarity, similarityActive],
  );

  const filterByTag = useCallback(
    (tagId: number, action: TagFilterAction, scope: TagFilterScope) => {
      const currentState = browseRef.current;
      const nextState = tagFilterState(currentState, {
        tagId,
        action,
        scope,
        collectionId: effectiveCollectionId,
      });
      if (!nextState) return;

      const targetId = activeIdRef.current;
      const targetRow =
        targetId == null ? undefined : itemsRef.current.find((item) => item.id === targetId);
      // 取消标签会放宽结果、追加/替换会带上当前图自身的标签，只有前者无法确认目标图
      // 仍在结果里；相似/语义结果不是图库视图，切回普通视图时同样无法确认。这两种情况
      // 交给常规清空，避免把不在结果里的行插进列表。
      const keepTarget =
        action !== "remove" &&
        !(similarityActive && action === "append") &&
        targetRow != null &&
        (action !== "apply" ||
          rowInTagScope(targetRow, scope, {
            folderPaths: folders
              .filter((folder) => currentState.filters.folderIds.includes(folder.id))
              .map((folder) => folder.path),
            folderDirect: currentState.filters.folderDirect,
            hasCollection: effectiveCollectionId != null,
            similarityActive,
          }));

      const nextKey = queryKey(buildQuery(nextState));
      if (keepTarget && targetRow && nextKey !== queryKey(buildQuery(currentState))) {
        preserveTagSelectionRef.current = true;
        pendingTagFocusRef.current = {
          id: targetRow.id,
          row: targetRow,
          jumpTried: false,
          queryKey: nextKey,
        };
      } else {
        preserveTagSelectionRef.current = false;
        pendingTagFocusRef.current = null;
      }
      applyBrowse(nextState);
      similarityClear?.();
    },
    [applyBrowse, effectiveCollectionId, folders, similarityActive, similarityClear],
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
        inTemp: effectiveView === "temp",
        expectTrashed: effectiveView === "trash",
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
        onSaved: (imageId, update) => patchSavedImage(imageId, update),
        refresh: (change) => applyChange(change),
        filterByTag,
        folderScopeLabel,
        collectionScopeLabel,
        filterTagIds: browseRef.current.filters.tagIds,
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
      collectionScopeLabel,
      deleteSelected,
      effectiveCollectionId,
      filterByTag,
      folderScopeLabel,
      folders,
      loadMore,
      onOpenViewer,
      patchSavedImage,
      applyChange,
      removeFromCurrentCollection,
      restoreSelected,
      trashSelected,
    ],
  );

  /** 打开“找相似”的比较图：目标图不在结果里，单独用它的详情组装查看器。 */
  const openSimilarityTarget = useCallback(async () => {
    const targetId = similarity?.targetIds[0];
    if (targetId == null) return;
    let details = similarityTarget?.id === targetId ? similarityTarget : null;
    if (!details) {
      try {
        details = await libraryApi.getImageDetail(targetId);
      } catch (cause) {
        notify("打开比较图失败", "error", errorMessage(cause));
        return;
      }
    }
    if (!details) return;
    const row: ImageRow = {
      id: details.id,
      contentHash: details.contentHash,
      fileExt: details.fileExt,
      folderId: details.folderId,
      folderPath: details.folderPath,
      folderTrashed: details.folderTrashed,
      filePath: details.filePath,
      fileSize: details.fileSize,
      width: details.width,
      height: details.height,
      format: details.format,
      hasAlpha: details.hasAlpha,
      score: details.score,
      importedAt: details.importedAt,
      modifiedAt: details.modifiedAt,
      deletedAt: details.deletedAt,
      thumbVersion: details.thumbVersion,
      thumbWidth: details.thumbWidth,
      thumbHeight: details.thumbHeight,
      tags: details.tags.map((tag) => tag.name),
    };
    targetViewerItems.current = [row];
    const session: ViewerSession = {
      initialId: row.id,
      folders,
      currentCollectionId: null,
      inTemp: false,
      expectTrashed: false,
      getItems: () => targetViewerItems.current,
      hasMore: () => false,
      subscribe: () => () => {},
      loadMore: () => {},
      onActiveChange: () => {},
      onSaved: (imageId, update) => patchSavedImage(imageId, update),
      refresh: (change) => applyChange(change),
      filterByTag,
      folderScopeLabel,
      collectionScopeLabel,
      filterTagIds: browseRef.current.filters.tagIds,
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
  }, [
    collectionScopeLabel,
    deleteSelected,
    filterByTag,
    folderScopeLabel,
    folders,
    notify,
    onOpenViewer,
    patchSavedImage,
    applyChange,
    removeFromCurrentCollection,
    restoreSelected,
    similarity?.targetIds,
    similarityTarget,
    trashSelected,
  ]);

  const handleCollectionAdded = useCallback(() => {
    applyChange({
      kind: "collection",
      source: "local",
      reason: "collectionAdd",
      similarity: "keep",
    });
  }, [applyChange]);

  const handleCollectionCreated = useCallback(
    (collection: CollectionInfo) => {
      setCreateCollectionOpen(false);
      setCreatePreset(null);
      clearSelection();
      void reloadCollections();
      applyBrowse({
        ...browseRef.current,
        ...sortForViewChange(browseRef.current, "collection"),
        view: "collection",
        collectionId: collection.id,
        searchScope: "view",
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
            ...sortForViewChange(browseRef.current, "all"),
            view: "all",
            collectionId: null,
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

  const handleTagChanged = useCallback(
    (info?: TagChange) => {
      applyChange({
        kind: "tagCatalog",
        source: "local",
        reason: info?.action ?? "tagCatalog",
        tagId: info?.tagId,
        tagAction: info?.action ?? "rename",
        targetTagId: info?.targetTagId,
        similarity: "keep",
      });
    },
    [applyChange],
  );

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
      const leavingTemp = currentState.view === "temp";
      applyBrowse({
        ...currentState,
        ...(leavingTemp ? sortForViewChange(currentState, "all") : {}),
        view: leavingTemp ? "all" : currentState.view,
        collectionId: leavingTemp ? null : currentState.collectionId,
        filters: {
          ...currentState.filters,
          folderIds: folderId == null ? [] : [folderId],
          folderDirect: false,
          trashLoose: false,
        },
      });
      // 面包屑切文件夹同样要离开相似/语义结果，否则网格还是旧文件夹的结果。
      similarityClear?.();
    },
    [applyBrowse, similarityClear],
  );

  /** 从详情跳到所在文件夹：退出回收站/临时视图，清空搜索与其它筛选。 */
  const openFolderLocation = useCallback(
    (folderId: number) => {
      applyBrowse(browseToFolder(browseRef.current, folderId));
      clearSelection();
      similarityClear?.();
    },
    [applyBrowse, clearSelection, similarityClear],
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
      else if (findSimilarOpen) setFindSimilarOpen(false);
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
      if (effectiveView === "temp") {
        notify("临时图片不能移入回收站", "info", "关闭、切换图库或退出应用时会自动清空。");
      } else if (effectiveView === "trash") void deleteSelected(ids);
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
    if (browse.filters.folderIds.length > 0) {
      parts.push(
        `文件夹 ${browse.filters.folderIds.length}${browse.filters.folderDirect ? "（直属）" : ""}`,
      );
    }
    if (browse.filters.extensions.length > 0) parts.push(`扩展名 ${browse.filters.extensions.length}`);
    if (browse.filters.scores.length > 0) parts.push(`评分 ${browse.filters.scores.length}`);
    if (browse.filters.tagIds.length > 0) {
      parts.push(
        `标签 ${browse.filters.tagIds.length}（${browse.filters.tagMode === "all" ? "全部" : "任一"}）`,
      );
    }
    if (browse.filters.excludeTagIds.length > 0) {
      parts.push(`不含 ${browse.filters.excludeTagIds.length} 个标签`);
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
  const similarityReferenceCount = similarityReference?.ids.length ?? 0;
  const referenceCount = similarity?.targetIds.length ?? 0;
  const averageReference = referenceCount > 1;
  const similarityTargetLabel = averageReference
    ? `${referenceCount} 张参考图`
    : similarityTarget
      ? fileNameOf(similarityTarget.filePath)
      : similarityTargetId != null
        ? `#${similarityTargetId}`
        : "比较图";
  const noResults = status === "ready" && items.length === 0;
  const selectedIds = useMemo(() => [...selected], [selected]);
  const contextHasDeleted = contextMenu?.items.some((item) => item.deletedAt != null) ?? false;
  const contextHasActive = contextMenu?.items.some((item) => item.deletedAt == null) ?? false;
  const contextItems = contextMenu?.items ?? [];
  const contextDeletedUnderFolder =
    contextHasDeleted &&
    contextItems.length > 0 &&
    contextItems
      .filter((item) => item.deletedAt != null)
      .every((item) => item.folderTrashed);

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
          trashFolderCount={trashFolders?.folders.length ?? 0}
          tempCount={current?.tempCount ?? 0}
          emptyingTrash={emptyingTrash}
          onShowAll={showAll}
          onShowTrash={showTrash}
          onShowTemp={showTemp}
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
          <button
            ref={historyAnchorRef}
            type="button"
            className="button button-secondary button-sm"
            aria-expanded={historyOpen}
            disabled={activeTaskCount > 0}
            title={
              activeTaskCount > 0
                ? `「${activeTaskLabel ?? "其他任务"}」正在进行，完成后才能撤销`
                : "最近的操作记录"
            }
            onClick={() => setHistoryOpen((open) => !open)}
          >
            <History size={14} />
            记录
          </button>
          <OperationHistoryPanel
            open={historyOpen}
            anchorRef={historyAnchorRef}
            onClose={() => setHistoryOpen(false)}
          />
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
                disabled={!onOpenOptimize}
                onClick={() => void openOptimize()}
              >
                <HardDrive size={14} />
                <span>优化图片空间…</span>
              </button>
              <button
                type="button"
                className="context-menu-item"
                disabled={!onOpenSimilarityGroups}
                onClick={() => void openSimilarityGroups()}
              >
                <ScanSearch size={14} />
                <span>相似分组…</span>
              </button>
            </MenuSection>
            <MenuSection title="维护">
              <button
                type="button"
                className="context-menu-item"
                disabled={!onOpenMaintenance}
                onClick={() => void openMaintenance()}
              >
                <ShieldCheck size={14} />
                <span>维护与检查…</span>
              </button>
            </MenuSection>
          </Popover>
        </div>
      </div>

      <div className="library-toolbar">
        <div className="folder-breadcrumbs" aria-label="当前文件夹">
          <button
            type="button"
            className={`folder-crumb${
              browse.filters.folderIds.length === 0 && !browse.filters.trashLoose
                ? " is-current"
                : ""
            }`}
            onClick={() => showFolder(null)}
          >
            <FolderOpen size={13} />
            {effectiveView === "temp"
              ? "临时"
              : effectiveView === "trash"
                ? "回收站"
                : "全部图片"}
          </button>
          {effectiveView !== "temp" && browse.filters.folderIds.length > 1 ? (
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
                    filters: {
                      ...browseRef.current.filters,
                      folderIds: [],
                      folderDirect: false,
                    },
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
                  className={`folder-crumb${
                    index === trail.length - 1 && !browse.filters.folderDirect
                      ? " is-current"
                      : ""
                  }`}
                  onClick={() => showFolder(crumb.id)}
                  disabled={crumb.id == null}
                  title={crumb.path}
                >
                  {crumb.label}
                </button>
              </span>
            ))
          )}
          {(browse.filters.folderDirect && browse.filters.folderIds.length === 1) ||
          browse.filters.trashLoose ? (
            <>
              <ChevronRight size={12} className="folder-crumb-separator" />
              <span className="folder-crumb is-current">直属图片</span>
            </>
          ) : null}
        </div>

        <div className="library-toolbar-right">
          <div className="search-field">
            <Search size={14} />
            <div className="search-mode" role="group" aria-label="搜索模式">
              <button
                type="button"
                className={semanticMode ? "" : "is-active"}
                aria-pressed={!semanticMode}
                title="匹配描述、标签与文件夹名称"
                onClick={() => switchSearchMode("match")}
              >
                匹配
              </button>
              <button
                type="button"
                className={semanticMode ? "is-active" : ""}
                aria-pressed={semanticMode}
                disabled={!similarity || Boolean(semanticBlockedReason)}
                title={semanticBlockedReason ?? "用一句话描述想找的画面；英文描述命中更准"}
                onClick={() => switchSearchMode("semantic")}
              >
                语义
              </button>
            </div>
            <input
              ref={searchInputRef}
              className="input"
              value={semanticMode ? semanticDraft : searchDraft}
              placeholder={
                semanticMode
                  ? "a woman in a red cloak, side view（英文更准）"
                  : "搜索描述、标签、文件夹"
              }
              aria-label={semanticMode ? "语义搜索" : "搜索图片"}
              disabled={
                semanticMode ? !similarity || Boolean(semanticBlockedReason) : similarityActive
              }
              onChange={(event) =>
                semanticMode
                  ? setSemanticDraft(event.target.value)
                  : setSearchDraft(event.target.value)
              }
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  // 只清空搜索框，不触发容器的“清空选择”。
                  event.stopPropagation();
                  if (semanticMode) setSemanticDraft("");
                  else setSearchDraft("");
                  event.currentTarget.blur();
                }
                if (event.key === "Enter" && semanticMode) {
                  event.preventDefault();
                  submitSemanticSearch();
                }
              }}
            />
            {semanticMode && semanticDraft ? (
              <IconButton label="清除语义搜索" onClick={() => setSemanticDraft("")}>
                <X size={12} />
              </IconButton>
            ) : null}
            {!semanticMode && searchDraft ? (
              <IconButton label="清除搜索" onClick={() => setSearchDraft("")}>
                <X size={12} />
              </IconButton>
            ) : null}
            {!semanticMode && searchDraft.trim() ? (
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
            {semanticMode ? (
              <IconButton
                label="开始语义搜索"
                disabled={!semanticDraft.trim() || Boolean(semanticBlockedReason) || similarity?.busy}
                onClick={submitSemanticSearch}
              >
                {similarity?.busy ? <Spinner size={13} /> : <Sparkles size={13} />}
              </IconButton>
            ) : null}
          </div>

          <div className="filter-anchor" ref={findSimilarAnchorRef}>
            <Button
              size="sm"
              variant={similarityReference ? "primary" : "secondary"}
              icon={<ScanSearch size={14} />}
              disabled={!similarityReference}
              aria-expanded={findSimilarOpen}
              title={
                similarityReference
                  ? similarityReferenceCount > 1
                    ? `已设 ${similarityReferenceCount} 张相似参考（平均），点击调整条件`
                    : "已设 1 张相似参考，点击调整条件"
                  : "右键图片设为相似参考"
              }
              onClick={toggleFindSimilarPanel}
            >
              相似
              {similarityReferenceCount > 1 ? ` · ${similarityReferenceCount}` : ""}
            </Button>
            <FindSimilarPanel
              open={findSimilarOpen}
              reference={similarityReference}
              context={findSimilarContext}
              initial={findSimilarInitial}
              onClose={() => setFindSimilarOpen(false)}
              onStart={startFindSimilar}
            />
          </div>

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
              tagOptionsVersion={tagOptionsVersion}
              featureSummary={featureSummary}
              featureSummaryLoading={featureSummaryLoading}
              onFiltersChange={(filters) =>
                applyBrowse({ ...browseRef.current, filters })
              }
              onClose={() => setFilterOpen(false)}
              onReset={resetFilters}
              onManageTags={() => setTagManagerOpen(true)}
            />
          </div>

          <SortMenu
            sort={browse.sort}
            profiles={settings?.ui.sortProfiles ?? []}
            view={effectiveViewOf(browse)}
            disabled={similarityActive}
            onSortChange={applySort}
            onProfilesChange={(sortProfiles) => persistUi({ sortProfiles })}
          />

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

      {similarityActive && similarity ? (
        <div className="filter-summary semantic-summary">
          {similarMode ? <ScanSearch size={14} /> : <Sparkles size={14} />}
          <span>
            {similarMode ? (
              <>
                相似于{" "}
                <button
                  type="button"
                  className="similarity-target"
                  title={averageReference ? "打开第一张参考图" : "打开这张比较图"}
                  onClick={() => void openSimilarityTarget()}
                >
                  {similarityTargetLabel}
                </button>
                {similarity.kind
                  ? ` · ${similarKindLabel(similarity.kind, averageReference)}`
                  : ""}
                {similarity.scopeLabel ? ` · 范围：${similarity.scopeLabel}` : ""}
              </>
            ) : (
              `语义搜索「${similarity.text}」`
            )}{" "}
            · {similarity.total.toLocaleString()} 张结果
          </span>
          {similarMode && similarityReference ? (
            <Button
              size="sm"
              variant="ghost"
              icon={<SlidersHorizontal size={12} />}
              onClick={openFindSimilarFromToolbar}
            >
              调整
            </Button>
          ) : null}
          <Button
            size="sm"
            variant="success"
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
            <Button
              size="sm"
              variant="secondary"
              icon={<FilterX size={12} />}
              onClick={resetFilters}
            >
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
                {similarityActive
                  ? similarMode
                    ? "正在搜索…"
                    : (similarity.stage ?? "首次搜索需要先加载文本模型，请稍候。")
                  : "正在读取…"}
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
              anchorRevision={imageQuery.replaceRevision}
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
              onItemPointerDown={handleItemPointerDown}
              onBackgroundClick={handleBackgroundClick}
              onLayout={setGridColumns}
            />
          ) : null}

          {items.length > 0 && viewMode === "list" ? (
            <VirtualList
              ref={viewRef}
              items={items}
              columns={listColumns}
              onColumnsChange={(next) => persistUi({ listColumns: next })}
              resetKey={resetSignature}
              anchorRevision={imageQuery.replaceRevision}
              selected={selected}
              activeId={activeId}
              sort={browse.sort}
              sortDisabled={similarityActive}
              hasMore={hasMore}
              moreStatus={moreStatus}
              moreError={moreError}
              onSortChange={handleColumnSort}
              onLoadMore={loadMore}
              onRetryMore={retryMore}
              onItemClick={handleItemClick}
              onItemActivate={activateItem}
              onItemContextMenu={handleItemContextMenu}
              onItemPointerDown={handleItemPointerDown}
              onBackgroundClick={handleBackgroundClick}
            />
          ) : null}
        </div>

        {inspectorOpen ? (
          <InspectorPanel
            selectedIds={selectedIds}
            version={detailVersion}
            tagVersion={tagVersion}
            folders={folders}
            currentCollectionId={effectiveCollectionId}
            inTemp={effectiveView === "temp"}
            inTrash={effectiveView === "trash"}
            onClose={() => setInspectorOpen(false)}
            onChanged={(change) => applyChange(change)}
            onAddToCollection={() => setCollectionPickerOpen(true)}
            onRemoveFromCollection={(ids) => void removeFromCurrentCollection(ids)}
            onTrash={(ids) => void trashSelected(ids)}
            onRestore={(ids) => void restoreSelected(ids)}
            onDelete={(ids) => void deleteSelected(ids)}
            onFilterTag={filterByTag}
            filterTagIds={browse.filters.tagIds}
            folderScopeLabel={folderScopeLabel}
            collectionScopeLabel={collectionScopeLabel}
            onGenerateDescription={onGenerateDescription}
            onOpenFolder={openFolderLocation}
            statsContext={folderStatsContext}
          />
        ) : null}
      </div>

      {contextMenu ? (
        <LibraryContextMenu
          menu={contextMenu}
          inCollection={effectiveView === "collection"}
          inTemp={effectiveView === "temp"}
          hasActive={contextHasActive}
          hasDeleted={contextHasDeleted}
          deletedUnderFolder={contextDeletedUnderFolder}
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
          onSetSimilarReference={() =>
            handleSetSimilarReference(contextMenu.ids, contextMenu.items)
          }
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
          applyChange({
            kind: "collection",
            source: "local",
            reason: "collectionCombine",
            similarity: "keep",
          });
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
