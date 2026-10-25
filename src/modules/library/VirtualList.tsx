import {
  forwardRef,
  useEffect,
  useImperativeHandle,
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
} from "./model";
import type { ImageRow, SortKey, SortSpec } from "./types";
import type { MoreStatus } from "./useImageQuery";
import type { VirtualViewHandle } from "./virtual";

interface VirtualListProps {
  items: ImageRow[];
  resetKey: string;
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

const PADDING = 8;
const ROW_HEIGHT = 44;
const HEADER_HEIGHT = 32;
const FOOTER_HEIGHT = 44;

export const VirtualList = forwardRef<VirtualViewHandle, VirtualListProps>(function VirtualList(
  {
    items,
    resetKey,
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
    setScrollTop(0);
  }, [resetKey]);

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
      <div className="virtual-list-head virtual-list-grid" role="row">
        <span role="columnheader" />
        {(
          [
            ["尺寸", "pixels"],
            ["格式", null],
            ["大小", "size"],
            ["评分", "score"],
            ["标签", null],
            ["导入时间", "importedAt"],
            ["文件夹", "folder"],
          ] as const
        ).map(([label, key]) => {
          if (!key) {
            return (
              <span key={label} role="columnheader">
                {label}
              </span>
            );
          }
          const level = sort.findIndex((spec) => spec.key === key);
          const active = level >= 0;
          return (
            <span
              key={label}
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
                {label}
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
            style={{ top: HEADER_HEIGHT + PADDING + index * ROW_HEIGHT }}
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
            <span className="list-numeric">{formatDimensions(item.width, item.height)}</span>
            <span>{extensionLabel(item.fileExt)}</span>
            <span className="list-numeric">{formatBytes(item.fileSize)}</span>
            <span className="list-score">{formatScore(item.score)}</span>
            <span className="list-tags">{item.tags.length > 0 ? item.tags.join(", ") : "—"}</span>
            <span className="list-numeric">{formatDate(item.importedAt)}</span>
            <span className="list-folder" title={item.folderPath}>
              {item.deletedAt != null ? "回收站 · " : ""}
              {folderLabel(item.folderPath)}
            </span>
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
    </div>
  );
});
