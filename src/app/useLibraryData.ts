import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type Dispatch,
  type SetStateAction,
} from "react";
import { libraryApi } from "../modules/library/api";
import type { DataChange, DataChangeSignal } from "../modules/library/dataChange";
import type { FolderInfo, TrashFolders } from "../modules/library/types";
import type { LibraryBrowseController } from "../modules/library/useLibraryBrowse";
import { errorMessage } from "../shared/errors";
import type { ViewerSession } from "../shared/viewerSession";
import { useNotices } from "../shared/notices";
import { useAppRuntime } from "./runtimeContext";
import { useTaskData } from "./taskContext";

export interface LibraryData {
  changeSignal: DataChangeSignal | null;
  emitChange: (change: DataChange) => void;
  folders: FolderInfo[];
  setFolders: Dispatch<SetStateAction<FolderInfo[]>>;
  reloadFolders: () => Promise<void>;
  trashFolders: TrashFolders | null;
  selectedImageIds: number[];
  setSelectedImageIds: Dispatch<SetStateAction<number[]>>;
  viewerSession: ViewerSession | null;
  setViewerSession: Dispatch<SetStateAction<ViewerSession | null>>;
}

/**
 * 图库数据的变化排队与去重、文件夹与回收站树、图片选择，以及会被数据变化关掉的查看器状态。
 */
export function useLibraryData(browse: LibraryBrowseController): LibraryData {
  const { current } = useAppRuntime();
  const { notify } = useNotices();
  const { lastFinished, lastChange } = useTaskData();

  const [changeSignal, setChangeSignal] = useState<DataChangeSignal | null>(null);
  const [viewerSession, setViewerSession] = useState<ViewerSession | null>(null);
  const [folders, setFolders] = useState<FolderInfo[]>([]);
  const [foldersLoaded, setFoldersLoaded] = useState(false);
  const [trashFolders, setTrashFolders] = useState<TrashFolders | null>(null);
  const [selectedImageIds, setSelectedImageIds] = useState<number[]>([]);

  const changeQueue = useRef<DataChange[]>([]);
  const changeScheduled = useRef(false);
  const lastEmittedChange = useRef<{ key: string; at: number } | null>(null);

  /**
   * 把变化排队后一次交给图库。任务结束与导入完成会各发一次同一条变化，短窗口内按内容去重；
   * 去重键只用 id 数量与首尾，不拼接可能上万条的 id 列表。
   */
  const emitChange = useCallback((change: DataChange) => {
    const ids = change.ids ?? [];
    const idKey = ids.length > 0 ? `${ids.length}:${ids[0]}:${ids[ids.length - 1]}` : "";
    const key = [
      change.kind,
      change.source,
      change.reason ?? "",
      idKey,
      change.fields?.join(",") ?? "",
      change.tagAction ?? "",
      change.tagId ?? "",
      change.score ?? "",
      change.targetFolderPath ?? "",
      change.similarity,
    ].join("|");
    const now = performance.now();
    if (
      lastEmittedChange.current?.key === key &&
      now - lastEmittedChange.current.at < 400
    ) {
      return;
    }
    lastEmittedChange.current = { key, at: now };
    changeQueue.current.push(change);
    if (changeScheduled.current) return;
    changeScheduled.current = true;
    queueMicrotask(() => {
      changeScheduled.current = false;
      const changes = changeQueue.current.splice(0);
      if (changes.length === 0) return;
      setChangeSignal((previous) => ({
        revision: (previous?.revision ?? 0) + 1,
        changes,
      }));
    });
  }, []);

  const foldersGeneration = useRef(0);
  const reloadFolders = useCallback(async () => {
    const generation = foldersGeneration.current + 1;
    foldersGeneration.current = generation;
    try {
      const next = await libraryApi.getFolders();
      if (generation !== foldersGeneration.current) return;
      setFolders(next);
      setFoldersLoaded(true);
    } catch (cause) {
      if (generation !== foldersGeneration.current) return;
      notify("读取文件夹失败", "error", errorMessage(cause));
    }
  }, [notify]);

  useEffect(() => {
    setTrashFolders(null);
    void reloadFolders();
  }, [reloadFolders, current?.id]);

  const trashGeneration = useRef(0);
  const reloadTrashFolders = useCallback(async () => {
    const generation = trashGeneration.current + 1;
    trashGeneration.current = generation;
    try {
      const next = await libraryApi.getTrashFolders();
      if (generation !== trashGeneration.current) return;
      setTrashFolders(next);
    } catch (cause) {
      if (generation !== trashGeneration.current) return;
      notify("读取回收站文件夹失败", "error", errorMessage(cause));
    }
  }, [notify]);

  useEffect(() => {
    if (browse.browse.view !== "trash") return;
    void reloadTrashFolders();
  }, [browse.browse.view, current?.id, reloadTrashFolders]);

  useEffect(() => {
    if (browse.browse.view !== "trash" || !changeSignal) return;
    void reloadTrashFolders();
  }, [changeSignal, browse.browse.view, reloadTrashFolders]);

  useEffect(() => {
    if (!foldersLoaded || browse.browse.view === "trash") return;
    const selected = browse.browse.filters.folderIds;
    if (selected.length === 0) return;
    const existing = new Set(folders.map((folder) => folder.id));
    const next = selected.filter((id) => existing.has(id));
    if (next.length !== selected.length) {
      browse.applyBrowse(
        {
          ...browse.browse,
          filters: {
            ...browse.browse.filters,
            folderIds: next,
            folderDirect: next.length > 0 && browse.browse.filters.folderDirect,
          },
        },
        false,
      );
    }
  }, [folders, foldersLoaded, browse]);

  // 回收站树刷新后，已恢复/永久删除的文件夹要从选择里移除。
  useEffect(() => {
    if (browse.browse.view !== "trash" || !trashFolders) return;
    const selected = browse.browse.filters.folderIds;
    if (selected.length === 0) return;
    const existing = new Set(trashFolders.folders.map((folder) => folder.id));
    const next = selected.filter((id) => existing.has(id));
    if (next.length !== selected.length) {
      browse.applyBrowse(
        {
          ...browse.browse,
          filters: {
            ...browse.browse.filters,
            folderIds: next,
            folderDirect: next.length > 0 && browse.browse.filters.folderDirect,
          },
        },
        false,
      );
    }
  }, [trashFolders, browse]);

  /** 任务改变图库数据后把变化交给浏览界面；文件夹/计数/特征由变化计划决定。 */
  const lastChangeRevision = useRef<number | null>(null);
  useEffect(() => {
    if (!lastChange) return;
    const previous = lastChangeRevision.current;
    lastChangeRevision.current = lastChange.revision;
    if (previous === lastChange.revision) return;
    if (lastChange.closeViewer) setViewerSession(null);
    // 同一时刻多个任务结束：只拿到最后一条时退化为保守的完整刷新。
    if (previous != null && lastChange.revision - previous > 1) {
      emitChange({
        kind: "unknown",
        source: "task",
        reason: "batched",
        clearSelection: true,
        similarity: "clear",
      });
      return;
    }
    emitChange(lastChange.change);
  }, [lastChange, emitChange]);

  // 撤销/重做可能删除或恢复当前查看的图片，完成后退出查看器，避免停在过期内容上。
  useEffect(() => {
    if (lastFinished?.kind === "undo" || lastFinished?.kind === "redo") {
      setViewerSession(null);
    }
  }, [lastFinished]);

  return {
    changeSignal,
    emitChange,
    folders,
    setFolders,
    reloadFolders,
    trashFolders,
    selectedImageIds,
    setSelectedImageIds,
    viewerSession,
    setViewerSession,
  };
}
