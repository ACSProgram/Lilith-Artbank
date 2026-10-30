import { useCallback, type Dispatch, type SetStateAction } from "react";
import { libraryApi } from "../modules/library/api";
import type { DataChange } from "../modules/library/dataChange";
import type { FolderChange, FolderInfo } from "../modules/library/types";
import {
  applyFolderChange,
  foldersById,
  isSameOrDescendant,
  outermostFolders,
  parentFolderPath,
} from "../modules/library/folderTreeModel";
import { errorMessage } from "../shared/errors";
import { useDragSort, type DragPayload, type DropTarget } from "../shared/useDragSort";
import { useNotices } from "../shared/notices";

interface FolderDragOptions {
  folders: FolderInfo[];
  setFolders: Dispatch<SetStateAction<FolderInfo[]>>;
  emitChange: (change: DataChange) => void;
}

export interface FolderDrag {
  beginDrag: (payload: DragPayload, event: React.PointerEvent) => void;
}

/** 图片与文件夹的拖放：图片移入文件夹、文件夹移动/排序，以及放置判定。 */
export function useFolderDrag({ folders, setFolders, emitChange }: FolderDragOptions): FolderDrag {
  const { notify } = useNotices();

  const handleDrop = useCallback(
    async (item: DragPayload, target: DropTarget) => {
      if (item.kind === "images") {
        if (target.position !== "inside") return;
        const path =
          target.kind === "root"
            ? ""
            : (folders.find((folder) => folder.id === target.folderId)?.path ?? "");
        try {
          const report = await libraryApi.moveImagesToFolder(item.imageIds, path);
          notify(
            `已移动 ${report.moved.toLocaleString()} 张，跳过 ${report.skipped.toLocaleString()} 张`,
            report.failures.length > 0 ? "error" : "success",
            report.failures[0]?.reason,
          );
          emitChange({
            kind: "folder",
            source: "local",
            reason: "moveImages",
            ids: item.imageIds,
            fields: ["folder"],
            targetFolderPath: path,
            similarity: "keep",
          });
        } catch (cause) {
          notify("移动图片失败", "error", errorMessage(cause));
        }
        return;
      }

      const dragged = outermostFolders(foldersById(folders, item.folderIds));
      if (dragged.length === 0) return;
      const targetFolder =
        target.kind === "root"
          ? folders.find((folder) => folder.path === "")
          : folders.find((folder) => folder.id === target.folderId);
      if (!targetFolder) return;
      if (dragged.some((folder) => isSameOrDescendant(targetFolder.path, folder.path))) {
        notify("不能把文件夹移动到自身或子文件夹", "error");
        return;
      }
      const draggedIds = [...dragged]
        .sort((left, right) => left.sortIndex - right.sortIndex || left.id - right.id)
        .map((folder) => folder.id);
      const movedIds = new Set(draggedIds);
      /** 目标父级下排除被拖文件夹后的同级顺序。 */
      const siblingsOf = (parentPath: string) =>
        folders
          .filter(
            (folder) =>
              folder.path !== "" &&
              parentFolderPath(folder.path) === parentPath &&
              !movedIds.has(folder.id),
          )
          .sort((left, right) => left.sortIndex - right.sortIndex || left.id - right.id);

      try {
        let change: FolderChange;
        if (target.position === "inside") {
          const alreadyInside = dragged.every(
            (folder) => parentFolderPath(folder.path) === targetFolder.path,
          );
          if (alreadyInside) {
            change = await libraryApi.reorderFolders(
              draggedIds,
              targetFolder.id,
              siblingsOf(targetFolder.path).length,
            );
          } else {
            change = await libraryApi.placeFolders(draggedIds, targetFolder.id, null);
          }
        } else {
          const parentPath = parentFolderPath(targetFolder.path);
          const parent = folders.find((folder) => folder.path === parentPath);
          if (!parent) return;
          const siblings = siblingsOf(parentPath);
          const position = siblings.findIndex((folder) => folder.id === targetFolder.id);
          if (position < 0) return;
          const index = target.position === "before" ? position : position + 1;
          const sameParent = dragged.every(
            (folder) => parentFolderPath(folder.path) === parentPath,
          );
          if (sameParent) {
            change = await libraryApi.reorderFolders(draggedIds, parent.id, index);
          } else {
            change = await libraryApi.placeFolders(draggedIds, parent.id, index);
          }
        }
        setFolders((previous) => applyFolderChange(previous, change));
        emitChange({
          kind: "folder",
          source: "local",
          reason: "moveFolder",
          fields: ["folder"],
          foldersAlreadyLoaded: true,
          similarity: "keep",
        });
      } catch (cause) {
        notify("移动文件夹失败", "error", errorMessage(cause));
      }
    },
    [folders, notify, emitChange],
  );

  const { beginDrag } = useDragSort({
    onDrop: (item, target) => handleDrop(item, target),
    onReject: (item, target) => {
      if (item.kind === "folders" && target.kind === "folder") {
        notify("不能把文件夹移动到自身或子文件夹", "error");
      }
    },
    canDrop: (item, target) => {
      if (item.kind === "images") return target.position === "inside";
      if (target.kind === "root") return true;
      const targetFolder = folders.find((folder) => folder.id === target.folderId);
      if (!targetFolder) return false;
      const dragged = foldersById(folders, item.folderIds);
      if (dragged.length !== item.folderIds.length) return false;
      return !dragged.some((folder) =>
        isSameOrDescendant(targetFolder.path, folder.path),
      );
    },
  });

  return { beginDrag };
}
