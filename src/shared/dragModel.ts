/** 指针拖动的载荷：图片集合或多选文件夹。 */
export type DragPayload =
  | { kind: "images"; imageIds: number[]; label: string }
  | { kind: "folders"; folderIds: number[]; label: string };

export type DropPosition = "before" | "inside" | "after";

export interface DropTarget {
  kind: "folder" | "root";
  folderId: number | null;
  position: DropPosition;
}

/** 图片只需要“移入”；文件夹拖动才需要上下边缘用于同级排序。 */
export function dropPositionFor(
  kind: DragPayload["kind"],
  ratio: number,
): DropPosition {
  if (kind === "images") return "inside";
  return ratio < 0.25 ? "before" : ratio > 0.75 ? "after" : "inside";
}

/** 直属图片行只接受图片，并且等同于拖到父文件夹。 */
export function directRowAccepts(kind: DragPayload["kind"]): boolean {
  return kind === "images";
}
