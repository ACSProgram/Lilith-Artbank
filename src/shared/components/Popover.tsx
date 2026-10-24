import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
  type RefObject,
} from "react";

export type PopoverPlacement = "bottom-start" | "bottom-end" | "top-start" | "top-end";

interface PopoverProps {
  open: boolean;
  /** 弹层挂在哪个元素下方；位置每次打开时按它的位置重算。 */
  anchorRef: RefObject<HTMLElement | null>;
  onClose: () => void;
  children: ReactNode;
  placement?: PopoverPlacement;
  /** 最小宽度；不传时按内容宽度。 */
  minWidth?: number;
  className?: string;
  /** 滚动时是否自动关闭，默认 `true`（排序/筛选这类弹层滚动后位置就失效了）。 */
  closeOnScroll?: boolean;
}

interface Position {
  left: number;
  top: number;
}

const MARGIN = 8;
const GAP = 6;

/**
 * 通用弹层：排序、筛选、菜单共用。
 * 定位用 fixed + 锚点矩形计算，内容超出视口时自动翻到上方并夹在边界内。
 */
export function Popover({
  open,
  anchorRef,
  onClose,
  children,
  placement = "bottom-start",
  minWidth,
  className,
  closeOnScroll = true,
}: PopoverProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState<Position | null>(null);

  useLayoutEffect(() => {
    if (!open) {
      setPosition(null);
      return;
    }
    const update = () => {
      const anchor = anchorRef.current;
      const panel = panelRef.current;
      if (!anchor || !panel) return;
      const rect = anchor.getBoundingClientRect();
      const width = panel.offsetWidth;
      const height = panel.offsetHeight;
      const maxLeft = Math.max(MARGIN, window.innerWidth - width - MARGIN);
      const maxTop = Math.max(MARGIN, window.innerHeight - height - MARGIN);

      const left0 = placement.endsWith("end") ? rect.right - width : rect.left;
      const left = Math.min(Math.max(MARGIN, left0), maxLeft);

      const below = rect.bottom + GAP;
      const above = rect.top - height - GAP;
      // 先按指定方向放，放不下翻到另一侧，两侧都放不下再夹进视口。
      let top = placement.startsWith("top") ? above : below;
      if (top + height > window.innerHeight - MARGIN) top = above;
      if (top < MARGIN) top = below;
      setPosition({ left, top: Math.min(Math.max(MARGIN, top), maxTop) });
    };

    update();
    // 首帧拿到的尺寸可能还不含异步内容，下一帧再校正一次。
    const frame = window.requestAnimationFrame(update);
    window.addEventListener("resize", update);
    return () => {
      window.cancelAnimationFrame(frame);
      window.removeEventListener("resize", update);
    };
  }, [open, anchorRef, placement]);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as Node;
      if (panelRef.current?.contains(target)) return;
      if (anchorRef.current?.contains(target)) return;
      onClose();
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.stopPropagation();
      onClose();
    };
    const onScroll = () => onClose();

    document.addEventListener("pointerdown", onPointerDown, true);
    window.addEventListener("keydown", onKeyDown, true);
    if (closeOnScroll) window.addEventListener("scroll", onScroll, true);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown, true);
      window.removeEventListener("keydown", onKeyDown, true);
      if (closeOnScroll) window.removeEventListener("scroll", onScroll, true);
    };
  }, [open, onClose, anchorRef, closeOnScroll]);

  if (!open) return null;

  return (
    <div
      ref={panelRef}
      className={["popover", className].filter(Boolean).join(" ")}
      style={{
        left: position?.left ?? 0,
        top: position?.top ?? 0,
        minWidth,
        visibility: position ? "visible" : "hidden",
      }}
      role="dialog"
    >
      {children}
    </div>
  );
}
