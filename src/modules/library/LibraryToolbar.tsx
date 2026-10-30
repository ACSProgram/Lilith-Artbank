import type { RefObject } from "react";
import {
  ArrowLeft,
  ChevronRight,
  Crop,
  FilterX,
  FolderOpen,
  HardDrive,
  History,
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
import { Button, IconButton } from "../../shared/components/Button";
import { MenuSection } from "../../shared/components/MenuSection";
import { Popover } from "../../shared/components/Popover";
import { Spinner } from "../../shared/components/Spinner";
import type { SortProfile, ThumbFit } from "../../shared/settings";
import type { BatchTaskContextInfo } from "../../shared/batchTaskContext";
import {
  similarKindLabel,
  type FindSimilarConfig,
} from "../../shared/similarity";
import {
  effectiveViewOf,
  hasActiveFilters,
  type BrowseState,
  type SimilarityBrowseState,
} from "./model";
import { FilterPanel } from "./FilterPanel";
import { FindSimilarPanel, type SimilarReference } from "./FindSimilarPanel";
import { OperationHistoryPanel } from "./OperationHistoryPanel";
import { SortMenu } from "./SortMenu";
import { ViewBar } from "./ViewBar";
import type {
  CollectionInfo,
  FeatureSummary,
  LibraryFilters,
  LibraryQuery,
  LibraryView as LibraryViewType,
  ViewMode,
} from "./types";

export type SearchMode = "match" | "semantic";

const THUMB_FITS: { value: ThumbFit; label: string; icon: typeof Crop }[] = [
  { value: "cover", label: "裁剪铺满", icon: Crop },
  { value: "contain", label: "完整显示", icon: Maximize },
];

interface LibraryTopBarProps {
  canGoBack: boolean;
  onGoBack: () => void;
  view: LibraryViewType;
  collectionId: number | null;
  collections: CollectionInfo[];
  imageCount: number;
  trashCount: number;
  trashFolderCount: number;
  tempCount: number;
  emptyingTrash: boolean;
  selectedCount: number;
  selectingAll: boolean;
  onClearSelection: () => void;
  onOpenImport?: () => void;
  taskAnchorRef: RefObject<HTMLButtonElement | null>;
  taskMenuOpen: boolean;
  onToggleTaskMenu: () => void;
  onCloseTaskMenu: () => void;
  historyAnchorRef: RefObject<HTMLButtonElement | null>;
  historyOpen: boolean;
  onToggleHistory: () => void;
  onCloseHistory: () => void;
  activeTaskCount: number;
  activeTaskLabel: string | null;
  onShowAll: () => void;
  onShowTrash: () => void;
  onShowTemp: () => void;
  onShowCollection: (collectionId: number) => void;
  onNewCollection: () => void;
  onManageCollections: () => void;
  onCombineCollections: () => void;
  onRenameCollection: (collection: CollectionInfo) => void;
  onDeleteCollection: (collection: CollectionInfo) => void;
  onMoveCollection: (collection: CollectionInfo, delta: number) => void;
  onDuplicateCollection: (collection: CollectionInfo) => void;
  onEmptyTrash: () => void;
  canOpenBatchTasks: boolean;
  onOpenBatchTasks: () => void;
  canOpenOptimize: boolean;
  onOpenOptimize: () => void;
  canOpenSimilarityGroups: boolean;
  onOpenSimilarityGroups: () => void;
  canOpenMaintenance: boolean;
  onOpenMaintenance: () => void;
}

/** 顶部视图栏与右侧动作：集合切换、导入、任务菜单与操作记录。 */
export function LibraryTopBar({
  canGoBack,
  onGoBack,
  view,
  collectionId,
  collections,
  imageCount,
  trashCount,
  trashFolderCount,
  tempCount,
  emptyingTrash,
  selectedCount,
  selectingAll,
  onClearSelection,
  onOpenImport,
  taskAnchorRef,
  taskMenuOpen,
  onToggleTaskMenu,
  onCloseTaskMenu,
  historyAnchorRef,
  historyOpen,
  onToggleHistory,
  onCloseHistory,
  activeTaskCount,
  activeTaskLabel,
  onShowAll,
  onShowTrash,
  onShowTemp,
  onShowCollection,
  onNewCollection,
  onManageCollections,
  onCombineCollections,
  onRenameCollection,
  onDeleteCollection,
  onMoveCollection,
  onDuplicateCollection,
  onEmptyTrash,
  canOpenBatchTasks,
  onOpenBatchTasks,
  canOpenOptimize,
  onOpenOptimize,
  canOpenSimilarityGroups,
  onOpenSimilarityGroups,
  canOpenMaintenance,
  onOpenMaintenance,
}: LibraryTopBarProps) {
  return (
    <div className="library-topbar">
      <IconButton label="返回上一视图" disabled={!canGoBack} onClick={onGoBack}>
        <ArrowLeft size={14} />
      </IconButton>
      <ViewBar
        view={view}
        collectionId={collectionId}
        collections={collections}
        imageCount={imageCount}
        trashCount={trashCount}
        trashFolderCount={trashFolderCount}
        tempCount={tempCount}
        emptyingTrash={emptyingTrash}
        onShowAll={onShowAll}
        onShowTrash={onShowTrash}
        onShowTemp={onShowTemp}
        onShowCollection={onShowCollection}
        onNewCollection={onNewCollection}
        onManageCollections={onManageCollections}
        onCombineCollections={onCombineCollections}
        onRenameCollection={onRenameCollection}
        onDeleteCollection={onDeleteCollection}
        onMoveCollection={onMoveCollection}
        onDuplicateCollection={onDuplicateCollection}
        onEmptyTrash={onEmptyTrash}
      />

      <div className="library-topbar-actions">
        {selectedCount > 0 ? (
          <div className="selection-summary">
            {selectingAll ? <Spinner size={13} /> : null}
            <span>已选 {selectedCount.toLocaleString()} 张</span>
            <IconButton label="清除选择" onClick={onClearSelection}>
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
          onClick={onToggleTaskMenu}
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
          onClick={onToggleHistory}
        >
          <History size={14} />
          记录
        </button>
        <OperationHistoryPanel
          open={historyOpen}
          anchorRef={historyAnchorRef}
          onClose={onCloseHistory}
        />
        <Popover
          open={taskMenuOpen}
          anchorRef={taskAnchorRef}
          placement="bottom-end"
          onClose={onCloseTaskMenu}
          minWidth={190}
        >
          <MenuSection title="工具">
            <button
              type="button"
              className="context-menu-item"
              disabled={!canOpenBatchTasks}
              onClick={onOpenBatchTasks}
            >
              <Sparkles size={14} />
              <span>批量任务…</span>
            </button>
            <button
              type="button"
              className="context-menu-item"
              disabled={!canOpenOptimize}
              onClick={onOpenOptimize}
            >
              <HardDrive size={14} />
              <span>优化图片空间…</span>
            </button>
            <button
              type="button"
              className="context-menu-item"
              disabled={!canOpenSimilarityGroups}
              onClick={onOpenSimilarityGroups}
            >
              <ScanSearch size={14} />
              <span>相似分组…</span>
            </button>
          </MenuSection>
          <MenuSection title="维护">
            <button
              type="button"
              className="context-menu-item"
              disabled={!canOpenMaintenance}
              onClick={onOpenMaintenance}
            >
              <ShieldCheck size={14} />
              <span>维护与检查…</span>
            </button>
          </MenuSection>
        </Popover>
      </div>
    </div>
  );
}

interface LibraryToolbarProps {
  browse: BrowseState;
  view: LibraryViewType;
  breadcrumbs: { id: number | null; label: string; path: string }[];
  onShowFolder: (folderId: number | null) => void;
  onClearFolders: () => void;
  searchInputRef: RefObject<HTMLInputElement | null>;
  semanticMode: boolean;
  searchDraft: string;
  semanticDraft: string;
  onSearchDraftChange: (value: string) => void;
  onSemanticDraftChange: (value: string) => void;
  onSwitchSearchMode: (mode: SearchMode) => void;
  onSubmitSemanticSearch: () => void;
  onSearchScopeChange: (scope: BrowseState["searchScope"]) => void;
  semanticBlockedReason: string | null;
  similarity?: SimilarityBrowseState;
  similarityActive: boolean;
  similarityReference: SimilarReference | null;
  similarityReferenceCount: number;
  findSimilarOpen: boolean;
  findSimilarContext: BatchTaskContextInfo | null;
  findSimilarInitial: FindSimilarConfig | null;
  findSimilarAnchorRef: RefObject<HTMLDivElement | null>;
  onToggleFindSimilar: () => void;
  onCloseFindSimilar: () => void;
  onStartFindSimilar: (config: FindSimilarConfig) => void;
  filterOpen: boolean;
  filterCount: number;
  filterAnchorRef: RefObject<HTMLDivElement | null>;
  onToggleFilter: () => void;
  onCloseFilter: () => void;
  onFiltersChange: (filters: LibraryFilters) => void;
  onResetFilters: () => void;
  onManageTags: () => void;
  query: LibraryQuery;
  tagOptionsVersion: number;
  featureSummary: FeatureSummary | null;
  featureSummaryLoading: boolean;
  sortProfiles: SortProfile[];
  onSortChange: (sort: BrowseState["sort"]) => void;
  onSortProfilesChange: (profiles: SortProfile[]) => void;
  viewMode: ViewMode;
  onViewModeChange: (mode: ViewMode) => void;
  thumbFit: ThumbFit;
  onThumbFitChange: (fit: ThumbFit) => void;
  inspectorOpen: boolean;
  onToggleInspector: () => void;
  onRetry: () => void;
}

/** 第二行工具栏：面包屑、搜索、找相似、筛选、排序与视图切换。 */
export function LibraryToolbar({
  browse,
  view,
  breadcrumbs,
  onShowFolder,
  onClearFolders,
  searchInputRef,
  semanticMode,
  searchDraft,
  semanticDraft,
  onSearchDraftChange,
  onSemanticDraftChange,
  onSwitchSearchMode,
  onSubmitSemanticSearch,
  onSearchScopeChange,
  semanticBlockedReason,
  similarity,
  similarityActive,
  similarityReference,
  similarityReferenceCount,
  findSimilarOpen,
  findSimilarContext,
  findSimilarInitial,
  findSimilarAnchorRef,
  onToggleFindSimilar,
  onCloseFindSimilar,
  onStartFindSimilar,
  filterOpen,
  filterCount,
  filterAnchorRef,
  onToggleFilter,
  onCloseFilter,
  onFiltersChange,
  onResetFilters,
  onManageTags,
  query,
  tagOptionsVersion,
  featureSummary,
  featureSummaryLoading,
  sortProfiles,
  onSortChange,
  onSortProfilesChange,
  viewMode,
  onViewModeChange,
  thumbFit,
  onThumbFitChange,
  inspectorOpen,
  onToggleInspector,
  onRetry,
}: LibraryToolbarProps) {
  return (
    <div className="library-toolbar">
      <div className="folder-breadcrumbs" aria-label="当前文件夹">
        <button
          type="button"
          className={`folder-crumb${
            browse.filters.folderIds.length === 0 && !browse.filters.trashLoose
              ? " is-current"
              : ""
          }`}
          onClick={() => onShowFolder(null)}
        >
          <FolderOpen size={13} />
          {view === "temp"
            ? "临时"
            : view === "trash"
              ? "回收站"
              : "全部图片"}
        </button>
        {view !== "temp" && browse.filters.folderIds.length > 1 ? (
          <>
            <ChevronRight size={12} className="folder-crumb-separator" />
            <span className="folder-crumb is-current">
              已选择 {browse.filters.folderIds.length.toLocaleString()} 个文件夹
            </span>
            <button
              type="button"
              className="text-button"
              onClick={onClearFolders}
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
                onClick={() => onShowFolder(crumb.id)}
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
              onClick={() => onSwitchSearchMode("match")}
            >
              匹配
            </button>
            <button
              type="button"
              className={semanticMode ? "is-active" : ""}
              aria-pressed={semanticMode}
              disabled={!similarity || Boolean(semanticBlockedReason)}
              title={semanticBlockedReason ?? "用一句话描述想找的画面；英文描述命中更准"}
              onClick={() => onSwitchSearchMode("semantic")}
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
                ? onSemanticDraftChange(event.target.value)
                : onSearchDraftChange(event.target.value)
            }
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                // 只清空搜索框，不触发容器的“清空选择”。
                event.stopPropagation();
                if (semanticMode) onSemanticDraftChange("");
                else onSearchDraftChange("");
                event.currentTarget.blur();
              }
              if (event.key === "Enter" && semanticMode) {
                event.preventDefault();
                onSubmitSemanticSearch();
              }
            }}
          />
          {semanticMode && semanticDraft ? (
            <IconButton label="清除语义搜索" onClick={() => onSemanticDraftChange("")}>
              <X size={12} />
            </IconButton>
          ) : null}
          {!semanticMode && searchDraft ? (
            <IconButton label="清除搜索" onClick={() => onSearchDraftChange("")}>
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
                onSearchScopeChange(browse.searchScope === "library" ? "view" : "library")
              }
            >
              {browse.searchScope === "library" ? "全库" : "当前视图"}
            </button>
          ) : null}
          {semanticMode ? (
            <IconButton
              label="开始语义搜索"
              disabled={!semanticDraft.trim() || Boolean(semanticBlockedReason) || similarity?.busy}
              onClick={onSubmitSemanticSearch}
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
            onClick={onToggleFindSimilar}
          >
            相似
            {similarityReferenceCount > 1 ? ` · ${similarityReferenceCount}` : ""}
          </Button>
          <FindSimilarPanel
            open={findSimilarOpen}
            reference={similarityReference}
            context={findSimilarContext}
            initial={findSimilarInitial}
            onClose={onCloseFindSimilar}
            onStart={onStartFindSimilar}
          />
        </div>

        <div className="filter-anchor" ref={filterAnchorRef}>
          <Button
            size="sm"
            variant={filterCount > 0 ? "primary" : "secondary"}
            icon={<SlidersHorizontal size={14} />}
            disabled={similarityActive}
            onClick={onToggleFilter}
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
            onFiltersChange={onFiltersChange}
            onClose={onCloseFilter}
            onReset={onResetFilters}
            onManageTags={onManageTags}
          />
        </div>

        <SortMenu
          sort={browse.sort}
          profiles={sortProfiles}
          view={effectiveViewOf(browse)}
          disabled={similarityActive}
          onSortChange={onSortChange}
          onProfilesChange={onSortProfilesChange}
        />

        <div className="segmented" role="group" aria-label="视图模式">
          <button
            type="button"
            className={viewMode === "grid" ? "is-active" : ""}
            title="网格视图"
            aria-label="网格视图"
            onClick={() => onViewModeChange("grid")}
          >
            <LayoutGrid size={15} />
          </button>
          <button
            type="button"
            className={viewMode === "list" ? "is-active" : ""}
            title="列表视图"
            aria-label="列表视图"
            onClick={() => onViewModeChange("list")}
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
              onClick={() => onThumbFitChange(value)}
            >
              <Icon size={15} />
            </button>
          ))}
        </div>

        <IconButton
          label={inspectorOpen ? "隐藏详情侧栏" : "显示详情侧栏"}
          className={inspectorOpen ? "is-active" : ""}
          onClick={onToggleInspector}
        >
          <PanelRight size={14} />
        </IconButton>

        <IconButton label="重新读取当前视图" onClick={onRetry}>
          <RefreshCw size={14} />
        </IconButton>
      </div>
    </div>
  );
}

interface LibrarySummariesProps {
  similarity?: SimilarityBrowseState;
  similarityReference: SimilarReference | null;
  averageReference: boolean;
  similarityTargetLabel: string;
  onOpenSimilarityTarget: () => void;
  onAdjustFindSimilar: () => void;
  filterSummary: string[];
  browse: BrowseState;
  onResetFilters: () => void;
  onClearSearch: () => void;
}

/** 工具栏下方的相似结果/筛选条件摘要条。 */
export function LibrarySummaries({
  similarity,
  similarityReference,
  averageReference,
  similarityTargetLabel,
  onOpenSimilarityTarget,
  onAdjustFindSimilar,
  filterSummary,
  browse,
  onResetFilters,
  onClearSearch,
}: LibrarySummariesProps) {
  const similarityActive = similarity?.active === true;
  const similarMode = similarity?.mode === "similar";
  return (
    <>
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
                  onClick={() => onOpenSimilarityTarget()}
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
              onClick={onAdjustFindSimilar}
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
              onClick={onResetFilters}
            >
              清除筛选
            </Button>
          ) : null}
          {browse.search ? (
            <Button size="sm" variant="ghost" onClick={onClearSearch}>
              清除搜索
            </Button>
          ) : null}
        </div>
      ) : null}
    </>
  );
}
