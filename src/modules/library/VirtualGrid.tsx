import {
  forwardRef,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from "react";
import { Check, RefreshCw } from "lucide-react";
import { Spinner } from "../../shared/components/Spinner";
import { LibraryThumbnail } from "./LibraryThumbnail";
import {
  extensionLabel,
  formatBytes,
  formatDimensions,
} from "./model";
import type { ImageRow } from "./types";
import type { MoreStatus } from "./useImageQuery";
import type { VirtualViewHandle } from "./virtual";

interface VirtualGridProps {
  items: ImageRow[];
  thumbPx: number;
  resetKey: string;
  selected: Set<number>;
  activeId: number | null;
  hasMore: boolean;
  moreStatus: MoreStatus;
  moreError: string | null;
  onLoadMore: () => void;
  onRetryMore: () => void;
  onItemClick: (item: ImageRow, event: ReactMouseEvent) => void;
  onItemActivate: (item: ImageRow) => void;
  onItemContextMenu: (item: ImageRow, event: ReactMouseEvent) => void;
  onBackgroundClick: () => void;
  onLayout: (columns: number) => void;
}

const PADDING = 18;
const GAP = 10;
const LABEL_HEIGHT = 38;
const FOOTER_HEIGHT = 46;

export const VirtualGrid = forwardRef<VirtualViewHandle, VirtualGridProps>(function VirtualGrid(
  {
    items,
    thumbPx,
    resetKey,
    selected,
    activeId,
    hasMore,
    moreStatus,
    moreError,
    onLoadMore,
    onRetryMore,
    onItemClick,
    onItemActivate,
    onItemContextMenu,
    onBackgroundClick,
    onLayout,
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

  const width = size.width || 1200;
  const columns = Math.max(
    1,
    Math.round((width - PADDING * 2 + GAP) / (thumbPx + GAP)),
  );
  const cellWidth = Math.max(72, (width - PADDING * 2 - GAP * (columns - 1)) / columns);
  const rowHeight = cellWidth + LABEL_HEIGHT + GAP;
  const rows = Math.ceil(items.length / columns);
  const bodyHeight = rows * rowHeight;
  const footerHeight = items.length > 0 ? FOOTER_HEIGHT : 0;
  const totalHeight = PADDING * 2 + bodyHeight + footerHeight;
  const viewportHeight = size.height || 600;
  const viewportRows = Math.max(1, Math.ceil(viewportHeight / rowHeight));
  const firstRow = Math.max(0, Math.floor(scrollTop / rowHeight) - viewportRows);
  const lastRow = Math.min(
    rows,
    Math.ceil((scrollTop + viewportHeight) / rowHeight) + viewportRows,
  );
  const startIndex = Math.min(items.length, firstRow * columns);
  const endIndex = Math.min(items.length, lastRow * columns);
  const nearEnd =
    size.height > 0 && scrollTop + viewportHeight >= totalHeight - viewportHeight * 1.5;

  useEffect(() => {
    onLayout(columns);
  }, [columns, onLayout]);

  useEffect(() => {
    if (!hasMore || moreStatus !== "idle" || !nearEnd) return;
    onLoadMore();
  }, [hasMore, moreStatus, nearEnd, onLoadMore, items.length]);

  const scrollToIndex = (index: number) => {
    const element = containerRef.current;
    if (!element || index < 0) return;
    const top = PADDING + Math.floor(index / columns) * rowHeight;
    const bottom = top + rowHeight;
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
    [columns, rowHeight, items],
  );

  return (
    <div
      className="virtual-grid"
      ref={containerRef}
      tabIndex={0}
      role="grid"
      aria-rowcount={rows}
      aria-colcount={columns}
      onClick={(event) => {
        if (event.target === event.currentTarget) onBackgroundClick();
      }}
      onContextMenu={(event) => {
        if (event.target === event.currentTarget) event.preventDefault();
      }}
    >
      <div className="virtual-space" style={{ height: totalHeight }} />
      {items.slice(startIndex, endIndex).map((item, offset) => {
        const index = startIndex + offset;
        const row = Math.floor(index / columns);
        const column = index % columns;
        const isSelected = selected.has(item.id);
        const isActive = activeId === item.id;
        return (
          <div
            key={item.id}
            tabIndex={-1}
            role="gridcell"
            aria-selected={isSelected}
            className={`grid-cell${isSelected ? " is-selected" : ""}${
              isActive ? " is-active" : ""
            }`}
            style={{
              width: cellWidth,
              height: cellWidth + LABEL_HEIGHT,
              transform: `translate3d(${PADDING + column * (cellWidth + GAP)}px, ${
                PADDING + row * rowHeight
              }px, 0)`,
            }}
            title={`${formatDimensions(item.width, item.height)} · ${extensionLabel(
              item.fileExt,
            )} · ${formatBytes(item.fileSize)}`}
            onClick={(event) => {
              containerRef.current?.focus({ preventScroll: true });
              onItemClick(item, event);
            }}
            onDoubleClick={() => onItemActivate(item)}
            onContextMenu={(event) => onItemContextMenu(item, event)}
          >
            <span className="grid-cell-image">
              <LibraryThumbnail image={item} />
            </span>
            <span className="grid-cell-label">
              <span>{formatDimensions(item.width, item.height)}</span>
              <span>
                {item.deletedAt != null ? "回收站 · " : ""}
                {extensionLabel(item.fileExt)} · {formatBytes(item.fileSize)}
              </span>
            </span>
            {item.score > 0 ? <span className="grid-cell-score">{item.score}</span> : null}
            {isSelected ? (
              <span className="grid-cell-check">
                <Check size={12} strokeWidth={3} />
              </span>
            ) : null}
          </div>
        );
      })}
      {items.length > 0 ? (
        <div
          className="virtual-footer"
          style={{ top: PADDING + bodyHeight, height: footerHeight }}
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
          ) : hasMore ? (
            <span>向下滚动载入更多</span>
          ) : (
            <span>已加载 {items.length.toLocaleString()} 张</span>
          )}
        </div>
      ) : null}
    </div>
  );
});
