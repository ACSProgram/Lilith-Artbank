import {
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
  type RefObject,
} from "react";

export const MIN_SPLIT_RATIO = 0.2;
export const MAX_SPLIT_RATIO = 0.8;

function clampSplitRatio(value: number): number {
  return Math.min(MAX_SPLIT_RATIO, Math.max(MIN_SPLIT_RATIO, value));
}

export interface FolderSplit {
  /** 拖动期间用草稿值，松手前不写回上层。 */
  ratio: number;
  dividerDragging: boolean;
  splitRef: RefObject<HTMLDivElement | null>;
  onDividerPointerDown: (event: ReactPointerEvent<HTMLDivElement>) => void;
  onDividerPointerMove: (event: ReactPointerEvent<HTMLDivElement>) => void;
  finishDividerDrag: (event: ReactPointerEvent<HTMLDivElement>) => void;
  onDividerKeyDown: (event: ReactKeyboardEvent<HTMLDivElement>) => void;
}

/**
 * 上下双视图的高度比例：拖动与方向键先落在本地草稿上，松手/按键时才回传上层。
 */
export function useFolderSplit(
  splitRatio: number,
  onSplitRatioChange: (ratio: number) => void,
): FolderSplit {
  const splitRef = useRef<HTMLDivElement>(null);
  const [ratioDraft, setRatioDraft] = useState<number | null>(null);
  const [dividerDragging, setDividerDragging] = useState(false);
  const dragRatioRef = useRef<number | null>(null);

  const ratio = ratioDraft ?? splitRatio;

  const onDividerPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    dragRatioRef.current = ratio;
    setRatioDraft(ratio);
    setDividerDragging(true);
  };

  const onDividerPointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (dragRatioRef.current == null) return;
    const rect = splitRef.current?.getBoundingClientRect();
    if (!rect || rect.height <= 0) return;
    const next = clampSplitRatio((event.clientY - rect.top) / rect.height);
    dragRatioRef.current = next;
    setRatioDraft(next);
  };

  const finishDividerDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
    const next = dragRatioRef.current;
    if (next == null) return;
    dragRatioRef.current = null;
    setDividerDragging(false);
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    if (next !== splitRatio) onSplitRatioChange(next);
  };

  const onDividerKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const step =
      event.key === "ArrowUp" ? -0.05 : event.key === "ArrowDown" ? 0.05 : 0;
    if (step === 0) return;
    event.preventDefault();
    const next = clampSplitRatio(ratio + step);
    setRatioDraft(next);
    onSplitRatioChange(next);
  };

  return {
    ratio,
    dividerDragging,
    splitRef,
    onDividerPointerDown,
    onDividerPointerMove,
    finishDividerDrag,
    onDividerKeyDown,
  };
}
