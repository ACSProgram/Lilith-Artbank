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
  CheckSquare,
  Copy,
  Eye,
  ImageOff,
  LayoutGrid,
  List,
  RefreshCw,
  SlidersHorizontal,
  X,
} from "lucide-react";
import { useAppRuntime } from "../../app/runtimeContext";
import { useNotices } from "../../app/noticeContext";
import { Button, IconButton } from "../../shared/components/Button";
import { Spinner } from "../../shared/components/Spinner";
import { copyText } from "../../shared/clipboard";
import { libraryApi } from "./api";
import { FilterPanel } from "./FilterPanel";
import {
  DEFAULT_SORT,
  activeFilterCount,
  buildQuery,
  emptyFilters,
  filterKey,
  formatDimensions,
  hasActiveFilters,
  isFormTarget,
  queryKey,
  selectionRange,
  sortOptions,
} from "./model";
import type { ImageRow, LibraryFilters, SortKey, SortSpec, ViewMode } from "./types";
import { useImageQuery } from "./useImageQuery";
import { VirtualGrid } from "./VirtualGrid";
import { VirtualList } from "./VirtualList";
import type { VirtualViewHandle } from "./virtual";

interface LibraryViewProps {
  refreshToken: number;
}

interface ContextMenuState {
  x: number;
  y: number;
  items: ImageRow[];
}

/** 右键菜单尺寸上限，用于把菜单夹在视口内（`.context-menu` 为 min-width 214px + 内边距）。 */
const CONTEXT_MENU_MAX_WIDTH = 264;
const CONTEXT_MENU_MAX_HEIGHT = 184;

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "未知错误";
}

function initialViewMode(): ViewMode {
  try {
    return localStorage.getItem("lilith.viewMode") === "list" ? "list" : "grid";
  } catch {
    return "grid";
  }
}

export function LibraryView({ refreshToken }: LibraryViewProps) {
  const { settings } = useAppRuntime();
  const { notify } = useNotices();

  const [viewMode, setViewMode] = useState<ViewMode>(initialViewMode);
  const [filters, setFilters] = useState<LibraryFilters>(emptyFilters);
  const [sort, setSort] = useState<SortSpec>(DEFAULT_SORT);
  const [filterOpen, setFilterOpen] = useState(false);
  const [selected, setSelected] = useState<Set<number>>(() => new Set());
  const [activeId, setActiveId] = useState<number | null>(null);
  const [anchorId, setAnchorId] = useState<number | null>(null);
  const [gridColumns, setGridColumns] = useState(1);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [selectingAll, setSelectingAll] = useState(false);

  const viewRef = useRef<VirtualViewHandle>(null);
  const filterAnchorRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const query = useMemo(() => buildQuery(filters, sort), [filters, sort]);
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
  } = useImageQuery(query, refreshToken);
  const resetSignature = useMemo(
    () => `${queryKey(query)}|${refreshToken}`,
    [query, refreshToken],
  );
  const selectionResetSignature = useMemo(
    () => `${filterKey(query)}|${refreshToken}`,
    [query, refreshToken],
  );
  const thumbPx = settings?.ui.gridThumbPx ?? 220;

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
    if (!contextMenu) return;
    const onPointerDown = (event: PointerEvent) => {
      if (!menuRef.current?.contains(event.target as Node)) setContextMenu(null);
    };
    const onScroll = () => setContextMenu(null);
    document.addEventListener("pointerdown", onPointerDown);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [contextMenu]);

  const clearSelection = useCallback(() => {
    setSelected(new Set());
    setActiveId(null);
    setAnchorId(null);
  }, []);

  const activateItem = useCallback(
    (item: ImageRow) => {
      setActiveId(item.id);
      setSelected(new Set([item.id]));
      setAnchorId(item.id);
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
      const targets = wasSelected ? items.filter((image) => selected.has(image.id)) : [item];
      if (!wasSelected) {
        setSelected(new Set([item.id]));
        setAnchorId(item.id);
      }
      setActiveId(item.id);
      setContextMenu({
        x: Math.min(event.clientX, Math.max(8, window.innerWidth - CONTEXT_MENU_MAX_WIDTH)),
        y: Math.min(event.clientY, Math.max(8, window.innerHeight - CONTEXT_MENU_MAX_HEIGHT)),
        items: targets.length > 0 ? targets : [item],
      });
    },
    [items, selected],
  );

  const handleBackgroundClick = useCallback(() => {
    setContextMenu(null);
    clearSelection();
  }, [clearSelection]);

  const selectAll = useCallback(async () => {
    if (selectingAll) return;
    setSelectingAll(true);
    try {
      const ids = await libraryApi.queryImageIds(query);
      setSelected(new Set(ids));
      const first = ids[0] ?? null;
      setAnchorId(first);
      setActiveId(first);
      notify(
        `已选择 ${ids.length.toLocaleString()} 张`,
        "info",
        ids.length === 0 ? "当前筛选没有图片" : "包含尚未滚动加载的图片",
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
      const hidden = selected.size > targets.length ? selected.size - targets.length : 0;
      notify(
        `已复制 ${targets.length.toLocaleString()} 条文件路径`,
        "success",
        hidden > 0 ? `另有 ${hidden.toLocaleString()} 张未加载，仅复制了已加载项` : undefined,
      );
    } catch (cause) {
      notify("复制路径失败", "error", errorMessage(cause));
    }
    setContextMenu(null);
  }, [contextMenu, notify, selected]);

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
    setFilters(emptyFilters());
  }, []);

  const handleKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const key = event.key;
    if (key === "Escape") {
      event.preventDefault();
      if (filterOpen) setFilterOpen(false);
      else if (contextMenu) setContextMenu(null);
      else clearSelection();
      return;
    }
    if (isFormTarget(event.target)) return;
    if (contextMenu) setContextMenu(null);

    if ((event.ctrlKey || event.metaKey) && key.toLowerCase() === "a") {
      event.preventDefault();
      void selectAll();
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
      notify("移入回收站将在 Stage 3 提供", "info");
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
    if (filters.folderIds.length > 0) parts.push(`文件夹 ${filters.folderIds.length}`);
    if (filters.extensions.length > 0) parts.push(`扩展名 ${filters.extensions.length}`);
    if (filters.scores.length > 0) parts.push(`评分 ${filters.scores.length}`);
    if (filters.tagIds.length > 0) {
      parts.push(`标签 ${filters.tagIds.length}（${filters.tagMode === "all" ? "全部" : "任一"}）`);
    }
    return parts;
  }, [filters]);

  const filterCount = activeFilterCount(filters);
  const noResults = status === "ready" && items.length === 0;

  return (
    <div className="library-view" onKeyDown={handleKeyDown}>
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
              filters={filters}
              query={query}
              onFiltersChange={setFilters}
              onClose={() => setFilterOpen(false)}
              onReset={resetFilters}
            />
          </div>

          <div className="sort-control">
            <select
              className="input sort-select"
              value={sort.key}
              aria-label="排序方式"
              onChange={(event) => setSort({ ...sort, key: event.target.value as SortKey })}
            >
              {sortOptions.map((option) => (
                <option key={option.key} value={option.key}>
                  {option.label}
                </option>
              ))}
            </select>
            <IconButton
              label={sort.descending ? "降序，点击切换为升序" : "升序，点击切换为降序"}
              onClick={() => setSort({ ...sort, descending: !sort.descending })}
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
          <button type="button" className="text-button" onClick={resetFilters}>
            清除筛选
          </button>
        </div>
      ) : null}

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
            <strong>没有符合条件的图片</strong>
            <span>
              {hasActiveFilters(filters)
                ? "调整筛选条件，或者清除筛选查看全部图片。"
                : "当前视图没有可显示的图片。"}
            </span>
            {hasActiveFilters(filters) ? (
              <Button size="sm" variant="primary" onClick={resetFilters}>
                清除筛选
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

      {contextMenu ? (
        <div
          ref={menuRef}
          className="context-menu"
          style={{ left: contextMenu.x, top: contextMenu.y }}
          onContextMenu={(event) => event.preventDefault()}
        >
          <button
            type="button"
            className="context-menu-item"
            onClick={() => {
              const item = contextMenu.items[0];
              setContextMenu(null);
              if (item) activateItem(item);
            }}
          >
            <Eye size={14} />
            <span>打开查看器</span>
            <em>Stage 4</em>
          </button>
          <button type="button" className="context-menu-item" onClick={() => void copyPaths()}>
            <Copy size={14} />
            <span>复制文件路径</span>
            {contextMenu.items.length > 1 ? <em>{contextMenu.items.length}</em> : null}
          </button>
          <div className="context-menu-separator" />
          <button
            type="button"
            className="context-menu-item"
            onClick={() => {
              setContextMenu(null);
              void selectAll();
            }}
          >
            <CheckSquare size={14} />
            <span>选择全部匹配项</span>
          </button>
        </div>
      ) : null}
    </div>
  );
}
