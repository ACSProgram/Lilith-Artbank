import { useCallback, useEffect, useRef } from "react";
import {
  directRowAccepts,
  dropPositionFor,
  type DragPayload,
  type DropPosition,
  type DropTarget,
} from "./dragModel";

export type { DragPayload, DropPosition, DropTarget } from "./dragModel";

interface DropOptions {
  onDrop: (item: DragPayload, target: DropTarget) => void | Promise<void>;
  /** 返回 false 时不显示放置高亮，松手也不会触发。 */
  canDrop?: (item: DragPayload, target: DropTarget) => boolean;
  /** 松手在不可放置的目标上时调用，用于说明原因。 */
  onReject?: (item: DragPayload, target: DropTarget) => void;
}

const START_DISTANCE = 6;
const EDGE = 44;
const SCROLL_STEP = 14;
const HOVER_EXPAND_MS = 600;

/** 悬停在文件夹行中间到达 `HOVER_EXPAND_MS` 后派发，由文件夹树展开该节点。 */
export const FOLDER_DRAG_HOVER_EVENT = "artbank:folder-drag-hover";

let swallowClickUntil = 0;

/** 拖动结束后紧跟的 click 不应该改变选择。 */
function swallowNextClick() {
  swallowClickUntil = Date.now() + 350;
}

function shouldIgnoreClick(event: MouseEvent) {
  if (Date.now() > swallowClickUntil) return false;
  event.preventDefault();
  event.stopPropagation();
  swallowClickUntil = 0;
  return true;
}

let clickGuardInstalled = false;

function installClickGuard() {
  if (clickGuardInstalled) return;
  clickGuardInstalled = true;
  window.addEventListener(
    "click",
    (event) => {
      shouldIgnoreClick(event);
    },
    true,
  );
}

interface Dragging {
  payload: DragPayload;
  pointerId: number;
  originX: number;
  originY: number;
  x: number;
  y: number;
  active: boolean;
  target: DropTarget | null;
  hoverFolderId: number | null;
  hoverSince: number;
}

/**
 * 指针事件拖放：HTML5 DnD 在启用 Tauri 原生拖放的 WebView2 里收不到事件，
 * 这里统一用 pointerdown / pointermove / pointerup，浏览器预览与真实窗口行为一致。
 */
export function useDragSort({ onDrop, canDrop, onReject }: DropOptions) {
  const dragRef = useRef<Dragging | null>(null);
  const ghostRef = useRef<HTMLDivElement | null>(null);
  const rafRef = useRef(0);
  const optionsRef = useRef({ onDrop, canDrop, onReject });
  optionsRef.current = { onDrop, canDrop, onReject };

  const clearHighlight = () => {
    document.querySelectorAll<HTMLElement>("[data-drop-position]").forEach((element) => {
      element.removeAttribute("data-drop-position");
    });
  };

  const removeGhost = () => {
    ghostRef.current?.remove();
    ghostRef.current = null;
    document.body.classList.remove("is-drag-sorting");
    cancelAnimationFrame(rafRef.current);
    rafRef.current = 0;
  };

  const resolveTarget = (
    x: number,
    y: number,
    kind: DragPayload["kind"],
  ): { target: DropTarget; element: HTMLElement } | null => {
    const element = document
      .elementFromPoint(x, y)
      ?.closest<HTMLElement>("[data-drop-root], [data-drop-folder], [data-drop-direct]");
    if (!element) return null;
    if (element.hasAttribute("data-drop-root")) {
      return { target: { kind: "root", folderId: null, position: "inside" }, element };
    }
    if (element.hasAttribute("data-drop-direct")) {
      if (!directRowAccepts(kind)) return null;
      const id = Number(element.dataset.dropDirect);
      if (!Number.isInteger(id) || id <= 0) return null;
      return { target: { kind: "folder", folderId: id, position: "inside" }, element };
    }
    const id = Number(element.dataset.dropFolder);
    if (!Number.isInteger(id) || id <= 0) return null;
    const rect = element.getBoundingClientRect();
    const ratio = rect.height > 0 ? (y - rect.top) / rect.height : 0.5;
    return {
      target: { kind: "folder", folderId: id, position: dropPositionFor(kind, ratio) },
      element,
    };
  };

  const applyHighlight = (element: HTMLElement, position: DropPosition) => {
    clearHighlight();
    element.setAttribute("data-drop-position", position);
  };

  const autoScroll = () => {
    const drag = dragRef.current;
    if (!drag?.active) return;
    const containers = document.querySelectorAll<HTMLElement>("[data-drag-scroll]");
    for (const container of containers) {
      const rect = container.getBoundingClientRect();
      if (drag.x < rect.left - 40 || drag.x > rect.right + 40) continue;
      if (drag.y > rect.top - EDGE && drag.y < rect.bottom + EDGE) {
        if (drag.y < rect.top + EDGE) container.scrollTop -= SCROLL_STEP;
        else if (drag.y > rect.bottom - EDGE) container.scrollTop += SCROLL_STEP;
      }
    }
    rafRef.current = requestAnimationFrame(autoScroll);
  };

  const moveGhost = (x: number, y: number) => {
    if (!ghostRef.current) {
      const ghost = document.createElement("div");
      ghost.className = "drag-ghost";
      document.body.appendChild(ghost);
      ghostRef.current = ghost;
    }
    ghostRef.current.textContent = dragRef.current?.payload.label ?? "";
    ghostRef.current.style.transform = `translate3d(${x + 14}px, ${y + 14}px, 0)`;
  };

  const finish = useCallback(() => {
    const drag = dragRef.current;
    dragRef.current = null;
    clearHighlight();
    removeGhost();
    window.removeEventListener("pointermove", onPointerMove);
    window.removeEventListener("pointerup", onPointerUp);
    window.removeEventListener("pointercancel", onPointerCancel);
    window.removeEventListener("keydown", onKeyDown, true);
    return drag;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const onPointerUp = useCallback(
    (event: PointerEvent) => {
      const drag = dragRef.current;
      if (!drag) return;
      if (event.pointerId !== drag.pointerId) return;
      const { payload, target, active } = drag;
      const accepted = active && target != null && (optionsRef.current.canDrop?.(payload, target) ?? true);
      const dropTarget = target;
      finish();
      if (!active) return;
      swallowNextClick();
      if (dropTarget) {
        if (accepted) void optionsRef.current.onDrop(payload, dropTarget);
        else optionsRef.current.onReject?.(payload, dropTarget);
      }
      event.preventDefault();
    },
    [finish],
  );

  const onPointerCancel = useCallback(
    (event: PointerEvent) => {
      if (event.pointerId !== dragRef.current?.pointerId) return;
      finish();
    },
    [finish],
  );

  const onKeyDown = useCallback(
    (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopPropagation();
      finish();
    },
    [finish],
  );

  const onPointerMove = useCallback(
    (event: PointerEvent) => {
      const drag = dragRef.current;
      if (!drag) return;
      if (event.pointerId !== drag.pointerId) return;
      const x = event.clientX;
      const y = event.clientY;
      drag.x = x;
      drag.y = y;
      if (!drag.active) {
        if (Math.hypot(x - drag.originX, y - drag.originY) < START_DISTANCE) return;
        drag.active = true;
        event.preventDefault();
        installClickGuard();
        document.body.classList.add("is-drag-sorting");
        rafRef.current = requestAnimationFrame(autoScroll);
      }
      moveGhost(x, y);

      const hit = resolveTarget(x, y, drag.payload.kind);
      if (!hit) {
        clearHighlight();
        drag.target = null;
        drag.hoverFolderId = null;
        drag.hoverSince = 0;
        return;
      }
      const accepted = optionsRef.current.canDrop?.(drag.payload, hit.target) ?? true;
      drag.target = hit.target;
      if (!accepted) {
        clearHighlight();
        drag.hoverFolderId = null;
        drag.hoverSince = 0;
        return;
      }
      applyHighlight(hit.element, hit.target.position);
      if (hit.target.kind === "folder" && hit.target.position === "inside") {
        const now = Date.now();
        if (drag.hoverFolderId !== hit.target.folderId) {
          drag.hoverFolderId = hit.target.folderId;
          drag.hoverSince = now;
        }
        if (now - drag.hoverSince > HOVER_EXPAND_MS) {
          window.dispatchEvent(
            new CustomEvent(FOLDER_DRAG_HOVER_EVENT, {
              detail: { folderId: hit.target.folderId },
            }),
          );
          drag.hoverSince = now;
        }
      } else {
        drag.hoverFolderId = null;
        drag.hoverSince = 0;
      }
    },
    [],
  );

  const beginDrag = useCallback(
    (payload: DragPayload, event: React.PointerEvent) => {
      if (event.button !== 0 || event.ctrlKey || event.metaKey || event.altKey) return;
      const target = event.target as HTMLElement | null;
      if (target?.closest("button, input, select, textarea, a")) return;
      dragRef.current = {
        payload,
        pointerId: event.pointerId,
        originX: event.clientX,
        originY: event.clientY,
        x: event.clientX,
        y: event.clientY,
        active: false,
        target: null,
        hoverFolderId: null,
        hoverSince: 0,
      };
      window.addEventListener("pointermove", onPointerMove);
      window.addEventListener("pointerup", onPointerUp);
      window.addEventListener("pointercancel", onPointerCancel);
      window.addEventListener("keydown", onKeyDown, true);
    },
    [onKeyDown, onPointerCancel, onPointerMove, onPointerUp],
  );

  useEffect(() => {
    return () => {
      window.removeEventListener("pointermove", onPointerMove);
      window.removeEventListener("pointerup", onPointerUp);
      window.removeEventListener("pointercancel", onPointerCancel);
      window.removeEventListener("keydown", onKeyDown, true);
      removeGhost();
    };
  }, [onKeyDown, onPointerCancel, onPointerMove, onPointerUp]);

  return { beginDrag };
}
