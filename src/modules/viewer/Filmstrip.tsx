import { useEffect, useRef, useState } from "react";
import { thumbnailUrl } from "../../shared/thumbUrl";
import type { ViewerSessionItem } from "../../shared/viewerSession";

const ITEM_WIDTH = 46;
const GAP = 6;
const STRIDE = ITEM_WIDTH + GAP;
const PADDING = 8;
/** 视口外多渲染几条，横向滚动时不出现空白。 */
const OVERSCAN = 8;

interface FilmstripProps {
  items: readonly ViewerSessionItem[];
  activeId: number;
  hasMore: boolean;
  onSelect: (imageId: number) => void;
  onLoadMore: () => void;
}

/** 当前视图的横向缩略图带；只渲染视口附近的条目。 */
export function Filmstrip({
  items,
  activeId,
  hasMore,
  onSelect,
  onLoadMore,
}: FilmstripProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  const [scrollLeft, setScrollLeft] = useState(0);
  const activeIndex = items.findIndex((item) => item.id === activeId);

  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    const measure = () => setWidth(element.clientWidth);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  // 滚轮转成横向滚动；React 的 onWheel 是被动监听，必须自己注册才能阻止默认行为。
  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    const onWheel = (event: WheelEvent) => {
      if (Math.abs(event.deltaY) <= Math.abs(event.deltaX)) return;
      event.preventDefault();
      element.scrollLeft += event.deltaY;
    };
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, []);

  // 当前项变化时居中；重新打开胶片带也回到当前图。
  useEffect(() => {
    const element = containerRef.current;
    if (!element || width <= 0 || activeIndex < 0) return;
    const target = PADDING + activeIndex * STRIDE - (width - ITEM_WIDTH) / 2;
    element.scrollLeft = Math.max(0, target);
    setScrollLeft(element.scrollLeft);
  }, [activeIndex, width]);

  useEffect(() => {
    if (!hasMore || items.length === 0) return;
    if (scrollLeft + width >= PADDING + items.length * STRIDE - STRIDE * 12) {
      onLoadMore();
    }
  }, [hasMore, items.length, onLoadMore, scrollLeft, width]);

  const start = Math.max(0, Math.floor(scrollLeft / STRIDE) - OVERSCAN);
  const end = Math.min(
    items.length,
    Math.ceil((scrollLeft + Math.max(width, 1)) / STRIDE) + OVERSCAN,
  );
  const trackWidth = PADDING * 2 + Math.max(items.length * STRIDE - GAP, 0);

  return (
    <div
      className="viewer-filmstrip"
      ref={containerRef}
      onScroll={(event) => setScrollLeft(event.currentTarget.scrollLeft)}
      role="listbox"
      aria-label="附近图片"
    >
      <div className="viewer-filmstrip-track" style={{ width: trackWidth }}>
        {items.slice(start, end).map((item, offset) => {
          const index = start + offset;
          return (
            <button
              key={item.id}
              type="button"
              className={`viewer-filmstrip-item${
                item.id === activeId ? " is-active" : ""
              }`}
              style={{ left: PADDING + index * STRIDE }}
              title={`#${item.id}`}
              role="option"
              aria-selected={item.id === activeId}
              onClick={() => onSelect(item.id)}
            >
              <img
                src={thumbnailUrl(item.id, item.thumbVersion)}
                alt=""
                draggable={false}
                loading="lazy"
                decoding="async"
              />
              <span className="viewer-filmstrip-index">{index + 1}</span>
            </button>
          );
        })}
      </div>
    </div>
  );
}
