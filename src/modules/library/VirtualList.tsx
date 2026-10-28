import {
  forwardRef,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";
import { RefreshCw } from "lucide-react";
import { Spinner } from "../../shared/components/Spinner";
import { LibraryThumbnail } from "./LibraryThumbnail";
import {
  extensionLabel,
  folderLabel,
  formatBytes,
  formatDate,
  formatDimensions,
  formatScore,
  isGifImage,
} from "./model";
import { LIST_COLUMNS, type ListColumn } from "../../app/types";
import type { ImageRow, SortKey, SortSpec } from "./types";
import type { MoreStatus } from "./useImageQuery";
import type { VirtualViewHandle } from "./virtual";

interface VirtualListProps {
  items: ImageRow[];
  /** 显示哪些列；缩略图列永远显示，不在其中。 */
  columns: ListColumn[];
  onColumnsChange: (columns: ListColumn[]) => void;
  resetKey: string;
  /** 数据刷新成功一次递增；用于把活动图按原视口偏移重新锚定。 */
  anchorRevision: number;
  selected: Set<number>;
  activeId: number | null;
  sort: SortSpec[];
  sortDisabled: boolean;
  hasMore: boolean;
  moreStatus: MoreStatus;
  moreError: string | null;
  onSortChange: (key: SortKey) => void;
  onLoadMore: () => void;
  onRetryMore: () => void;
  onItemClick: (item: ImageRow, event: ReactMouseEvent) => void;
  onItemActivate: (item: ImageRow) => void;
  onItemContextMenu: (item: ImageRow, event: ReactMouseEvent) => void;
  onItemPointerDown: (item: ImageRow, event: ReactPointerEvent<HTMLDivElement>) => void;
  onBackgroundClick: () => void;
}

/**
 * 列定义：键与 Rust `settings::LIST_COLUMNS` 一致，`sort` 为空表示这一列不参与排序。
 *
 * 宽度写在这里而不是 CSS 里：列可以隐藏，`grid-template-columns` 必须跟着变，
 * 固定写死会让隐藏后的单元格错位。标签列吃剩余宽度，其余定宽。
 */
const COLUMN_DEFS: {
  key: ListColumn;
  label: string;
  sort: SortKey | null;
  width: string;
  /** 容器窄于这个宽度时自动让出；与旧的响应式隐藏同一套阈值。 */
  hideBelow?: number;
}[] = [
  { key: "pixels", label: "尺寸", sort: "pixels", width: "100px" },
  { key: "format", label: "格式", sort: null, width: "52px" },
  { key: "size", label: "大小", sort: "size", width: "76px" },
  { key: "score", label: "评分", sort: "score", width: "64px" },
  { key: "tags", label: "标签", sort: null, width: "minmax(90px, 1fr)" },
  { key: "importedAt", label: "导入时间", sort: "importedAt", width: "132px", hideBelow: 900 },
  { key: "folder", label: "文件夹", sort: "folder", width: "130px", hideBelow: 1180 },
];

/** 单元格的类名；数字列统一用等宽数字，标签与文件夹另有配色。 */
const CELL_CLASS: Record<ListColumn, string> = {
  pixels: "list-numeric",
  format: "",
  size: "list-numeric",
  score: "list-score",
  tags: "list-tags",
  importedAt: "list-numeric",
  folder: "list-folder",
};

function cellTitle(key: ListColumn, item: ImageRow): string | undefined {
  if (key === "format" && isGifImage(item)) return "GIF 动图，仅在查看器中播放";
  if (key === "folder") return item.folderPath || "根目录";
  return undefined;
}

function renderCell(key: ListColumn, item: ImageRow) {
  switch (key) {
    case "pixels":
      return formatDimensions(item.width, item.height);
    case "format":
      return extensionLabel(item.fileExt);
    case "size":
      return formatBytes(item.fileSize);
    case "score":
      return formatScore(item.score);
    case "tags":
      return item.tags.length > 0 ? item.tags.join(", ") : "—";
    case "importedAt":
      return formatDate(item.importedAt);
    case "folder":
      return `${item.deletedAt != null ? "回收站 · " : ""}${folderLabel(item.folderPath)}`;
  }
}

const PADDING = 8;
const ROW_HEIGHT = 44;
const HEADER_HEIGHT = 32;
const FOOTER_HEIGHT = 44;

export const VirtualList = forwardRef<VirtualViewHandle, VirtualListProps>(function VirtualList(
  {
    items,
    columns,
    onColumnsChange,
    resetKey,
    anchorRevision,
    selected,
    activeId,
    sort,
    sortDisabled,
    hasMore,
    moreStatus,
    moreError,
    onSortChange,
    onLoadMore,
    onRetryMore,
    onItemClick,
    onItemActivate,
    onItemContextMenu,
    onItemPointerDown,
    onBackgroundClick,
  },
  ref,
) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  const [scrollTop, setScrollTop] = useState(0);
  /** 列头右键菜单的落点；相对容器，关闭时为 null。 */
  const [columnMenu, setColumnMenu] = useState<{ x: number; y: number } | null>(null);
  const scrollTopRef = useRef(0);
  const anchorRef = useRef({ revision: anchorRevision, items, scrollTop: 0 });

  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    const measure = () => setSize({ width: element.clientWidth, height: element.clientHeight });
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    let frame = 0;
    const onScroll = () => {
      if (frame) return;
      frame = requestAnimationFrame(() => {
        frame = 0;
        scrollTopRef.current = element.scrollTop;
        setScrollTop(element.scrollTop);
      });
    };
    element.addEventListener("scroll", onScroll, { passive: true });
    return () => {
      cancelAnimationFrame(frame);
      element.removeEventListener("scroll", onScroll);
    };
  }, []);

  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    element.scrollTop = 0;
    scrollTopRef.current = 0;
    setScrollTop(0);
  }, [resetKey]);

  useEffect(() => {
    if (!columnMenu) return;
    const close = (event: Event) => {
      if (event instanceof KeyboardEvent) {
        if (event.key === "Escape") setColumnMenu(null);
        return;
      }
      // 菜单自己身上的按下不算「点外面」，否则勾选框会在 click 触发前被卸载。
      if (event.target instanceof Element && event.target.closest(".list-column-menu")) return;
      setColumnMenu(null);
    };
    const closeOnScroll = () => setColumnMenu(null);
    window.addEventListener("pointerdown", close);
    window.addEventListener("keydown", close);
    containerRef.current?.addEventListener("scroll", closeOnScroll, { passive: true });
    return () => {
      window.removeEventListener("pointerdown", close);
      window.removeEventListener("keydown", close);
      containerRef.current?.removeEventListener("scroll", closeOnScroll);
    };
  }, [columnMenu]);

  const rows = items.length;
  const bodyHeight = rows * ROW_HEIGHT;
  const footerHeight = items.length > 0 ? FOOTER_HEIGHT : 0;
  const spacerHeight = PADDING * 2 + bodyHeight + footerHeight;
  const viewportHeight = size.height || 600;
  const contentScrollTop = Math.max(0, scrollTop - HEADER_HEIGHT);
  const viewportRows = Math.max(1, Math.ceil(viewportHeight / ROW_HEIGHT));
  const firstRow = Math.max(0, Math.floor(contentScrollTop / ROW_HEIGHT) - viewportRows);
  const lastRow = Math.min(
    rows,
    Math.ceil((contentScrollTop + viewportHeight) / ROW_HEIGHT) + viewportRows,
  );
  const nearEnd =
    size.height > 0 &&
    scrollTop + viewportHeight >= HEADER_HEIGHT + spacerHeight - viewportHeight * 1.5;

  useEffect(() => {
    if (!hasMore || moreStatus !== "idle" || !nearEnd) return;
    onLoadMore();
  }, [hasMore, moreStatus, nearEnd, onLoadMore, items.length]);

  /** 数据刷新替换 items 后，把活动行放回它原来的视口偏移。 */
  useLayoutEffect(() => {
    const element = containerRef.current;
    const previous = anchorRef.current;
    if (element && anchorRevision !== previous.revision && activeId != null) {
      const oldIndex = previous.items.findIndex((item) => item.id === activeId);
      const newIndex = items.findIndex((item) => item.id === activeId);
      if (oldIndex >= 0 && newIndex >= 0) {
        const oldTop = HEADER_HEIGHT + PADDING + oldIndex * ROW_HEIGHT;
        const offset = oldTop - previous.scrollTop;
        const newTop = HEADER_HEIGHT + PADDING + newIndex * ROW_HEIGHT;
        element.scrollTop = Math.max(0, newTop - offset);
        scrollTopRef.current = element.scrollTop;
        setScrollTop(element.scrollTop);
      }
    }
    anchorRef.current = {
      revision: anchorRevision,
      items,
      scrollTop: scrollTopRef.current,
    };
  });

  const scrollToIndex = (index: number) => {
    const element = containerRef.current;
    if (!element || index < 0) return;
    const top = HEADER_HEIGHT + PADDING + index * ROW_HEIGHT;
    const bottom = top + ROW_HEIGHT;
    if (top < element.scrollTop) {
      element.scrollTop = Math.max(0, top - 6);
    } else if (bottom > element.scrollTop + element.clientHeight) {
      element.scrollTop = bottom - element.clientHeight + 6;
    }
  };

  useImperativeHandle(
    ref,
    () => ({
      ensureIndex: (index) => scrollToIndex(index),
      scrollByPages: (pages) => {
        const element = containerRef.current;
        if (element) element.scrollBy({ top: element.clientHeight * 0.9 * pages });
      },
      scrollToTop: () => {
        const element = containerRef.current;
        if (element) element.scrollTo({ top: 0 });
      },
      scrollToBottom: () => {
        const element = containerRef.current;
        if (element) element.scrollTo({ top: element.scrollHeight });
      },
    }),
    [items],
  );

  // 容器太窄时自动让出右侧两列，和旧的媒体查询同一个阈值；还没量到宽度时全显示。
  const visibleColumns = COLUMN_DEFS.filter(
    (column) =>
      columns.includes(column.key) &&
      (column.hideBelow == null || size.width === 0 || size.width >= column.hideBelow),
  );
  const gridTemplate = ["38px", ...visibleColumns.map((column) => column.width)].join(" ");
  /** 勾选一列：至少留一列，顺序按 `LIST_COLUMNS` 归位，避免显示顺序随点击变化。 */
  const toggleColumn = (key: ListColumn) => {
    const shown = columns.includes(key);
    const next = shown
      ? columns.filter((item) => item !== key)
      : LIST_COLUMNS.filter((item) => item === key || columns.includes(item));
    if (next.length === 0) return;
    onColumnsChange(next);
  };

  return (
    <div
      className="virtual-list"
      ref={containerRef}
      tabIndex={0}
      role="grid"
      data-drag-scroll="true"
      onClick={(event) => {
        if (event.target === event.currentTarget) onBackgroundClick();
      }}
    >
      <div
        className="virtual-list-head virtual-list-grid"
        role="row"
        style={{ gridTemplateColumns: gridTemplate }}
        onContextMenu={(event) => {
          event.preventDefault();
          const rect = containerRef.current?.getBoundingClientRect();
          setColumnMenu({
            x: event.clientX - (rect?.left ?? 0) + (containerRef.current?.scrollLeft ?? 0),
            y: event.clientY - (rect?.top ?? 0) + (containerRef.current?.scrollTop ?? 0),
          });
        }}
      >
        <span role="columnheader" title="右键选择显示哪些列" />
        {visibleColumns.map((column) => {
          const key = column.sort;
          if (!key) {
            return (
              <span key={column.key} role="columnheader">
                {column.label}
              </span>
            );
          }
          const level = sort.findIndex((spec) => spec.key === key);
          const active = level >= 0;
          return (
            <span
              key={column.key}
              role="columnheader"
              aria-sort={
                active ? (sort[level].descending ? "descending" : "ascending") : "none"
              }
            >
              <button
                type="button"
                className={`list-sort${active ? " is-active" : ""}`}
                disabled={sortDisabled}
                title={active ? "点击切换方向；其它列点击改为按该列排序" : "按这一列排序"}
                onClick={() => onSortChange(key)}
              >
                {column.label}
                {active ? (
                  <span className="list-sort-arrow">
                    {sort[level].descending ? "↓" : "↑"}
                    {sort.length > 1 ? level + 1 : ""}
                  </span>
                ) : null}
              </button>
            </span>
          );
        })}
      </div>
      <div className="virtual-space" style={{ height: spacerHeight }} />
      {items.slice(firstRow, lastRow).map((item, offset) => {
        const index = firstRow + offset;
        const isSelected = selected.has(item.id);
        const isActive = activeId === item.id;
        return (
          <div
            key={item.id}
            role="row"
            aria-selected={isSelected}
            className={`virtual-list-row virtual-list-grid${isSelected ? " is-selected" : ""}${
              isActive ? " is-active" : ""
            }`}
            style={{
              top: HEADER_HEIGHT + PADDING + index * ROW_HEIGHT,
              gridTemplateColumns: gridTemplate,
            }}
            onClick={(event) => {
              containerRef.current?.focus({ preventScroll: true });
              onItemClick(item, event);
            }}
            onDoubleClick={() => onItemActivate(item)}
            onContextMenu={(event) => onItemContextMenu(item, event)}
            onPointerDown={(event) => onItemPointerDown(item, event)}
          >
            <span className="virtual-list-thumb">
              <LibraryThumbnail image={item} />
            </span>
            {visibleColumns.map((column) => (
              <span
                key={column.key}
                className={CELL_CLASS[column.key]}
                title={cellTitle(column.key, item)}
              >
                {renderCell(column.key, item)}
              </span>
            ))}
          </div>
        );
      })}
      {items.length > 0 ? (
        <div
          className="virtual-footer"
          style={{ top: HEADER_HEIGHT + PADDING + bodyHeight, height: footerHeight }}
        >
          {moreStatus === "loading" ? (
            <>
              <Spinner size={14} />
              <span>正在载入…</span>
            </>
          ) : moreStatus === "error" ? (
            <button
              type="button"
              className="virtual-footer-retry"
              title={moreError ?? undefined}
              onClick={onRetryMore}
            >
              <RefreshCw size={13} />
              加载失败，点击重试
            </button>
          ) : hasMore ? null : (
            <span>没有更多了</span>
          )}
        </div>
      ) : null}
      {columnMenu ? (
        <div
          className="list-column-menu"
          style={{ left: columnMenu.x, top: columnMenu.y }}
          role="menu"
          aria-label="显示哪些列"
        >
          <div className="list-column-menu-title">显示列</div>
          <label className="list-column-menu-item" title="缩略图列一直显示">
            <input type="checkbox" checked readOnly />
            <span>缩略图</span>
          </label>
          {COLUMN_DEFS.map((column) => {
            const shown = columns.includes(column.key);
            const last = shown && columns.length === 1;
            return (
              <label
                key={column.key}
                className="list-column-menu-item"
                title={last ? "至少要保留一列" : undefined}
              >
                <input
                  type="checkbox"
                  checked={shown}
                  disabled={last}
                  onChange={() => toggleColumn(column.key)}
                />
                <span>{column.label}</span>
              </label>
            );
          })}
        </div>
      ) : null}
    </div>
  );
});
