import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type Dispatch,
  type MutableRefObject,
  type SetStateAction,
} from "react";
import { libraryApi } from "../modules/library/api";
import type { CollectionInfo, FolderInfo, SortSpec } from "../modules/library/types";
import type { DataChange } from "../modules/library/dataChange";
import type { StatsStore } from "../modules/library/statsStore";
import { errorMessage } from "../shared/errors";
import type { BatchTaskContextInfo } from "../shared/batchTaskContext";
import { browserPreview } from "./api";
import { useNotices } from "../shared/notices";
import { useAppRuntime } from "./runtimeContext";

/** 重算对话框范围计数时用到的当前库数据。 */
interface ContextSources {
  folders: FolderInfo[];
  /** 为空表示这次没有重新读取集合，集合相关字段沿用旧值。 */
  collections: CollectionInfo[] | null;
  selectedIds: number[];
  libraryTotal: number | null;
  tempTotal: number | null;
}

/**
 * 按打开时的范围重取数量。范围对象已经消失时把对应字段置空，界面据此禁用该范围，
 * 不静默使用旧数字。
 */
async function refreshTaskContext(
  previous: BatchTaskContextInfo,
  sources: ContextSources,
  stats: StatsStore,
): Promise<BatchTaskContextInfo> {
  const folder =
    previous.folderId != null
      ? sources.folders.find((item) => item.id === previous.folderId)
      : undefined;
  const collections = sources.collections;
  const collection =
    previous.collectionId != null && collections
      ? collections.find((item) => item.id === previous.collectionId)
      : undefined;
  let viewTotal = previous.viewTotal;
  if (previous.query) {
    try {
      viewTotal = (await stats.fetch("summary", previous.query)).total;
    } catch {
      viewTotal = null;
    }
  }
  return {
    ...previous,
    selectedIds: sources.selectedIds,
    viewTotal,
    folderId: folder?.id ?? null,
    folderName: folder?.path ?? null,
    folderDirectTotal: folder?.directImageCount ?? null,
    folderTotal: folder?.totalImageCount ?? null,
    collectionId: collection?.id ?? (collections ? null : previous.collectionId),
    collectionName: collection?.name ?? (collections ? null : previous.collectionName),
    collectionTotal: collection?.imageCount ?? (collections ? null : previous.collectionTotal),
    libraryTotal: sources.libraryTotal ?? previous.libraryTotal,
    tempTotal: sources.tempTotal ?? previous.tempTotal,
  };
}

interface TaskDialogOptions {
  statsStore: StatsStore | null;
  emitChange: (change: DataChange) => void;
  folders: FolderInfo[];
  selectedImageIds: number[];
}

export interface TaskDialogs {
  importOpen: boolean;
  importOpenRef: MutableRefObject<boolean>;
  pendingPaths: string[];
  setPendingPaths: Dispatch<SetStateAction<string[]>>;
  setImportOpen: Dispatch<SetStateAction<boolean>>;
  openImport: (paths?: string[]) => void;
  closeImport: () => void;
  handleImportFinished: () => void;
  similarityGroupsOpen: boolean;
  setSimilarityGroupsOpen: Dispatch<SetStateAction<boolean>>;
  similarityGroupsTaskId: string | null;
  setSimilarityGroupsTaskId: Dispatch<SetStateAction<string | null>>;
  similarityGroupsContext: BatchTaskContextInfo | null;
  setSimilarityGroupsContext: Dispatch<SetStateAction<BatchTaskContextInfo | null>>;
  batchContext: BatchTaskContextInfo | null;
  setBatchContext: Dispatch<SetStateAction<BatchTaskContextInfo | null>>;
  optimizeContext: BatchTaskContextInfo | null;
  setOptimizeContext: Dispatch<SetStateAction<BatchTaskContextInfo | null>>;
  optimizeStale: boolean;
  setOptimizeStale: Dispatch<SetStateAction<boolean>>;
  maintenanceContext: BatchTaskContextInfo | null;
  setMaintenanceContext: Dispatch<SetStateAction<BatchTaskContextInfo | null>>;
  persistKeepSort: (next: SortSpec[]) => void;
  handleContextsChanged: () => void;
}

/** 批量任务、维护、优化、相似分组与导入对话框的开关和打开时携带的范围。 */
export function useTaskDialogs({
  statsStore,
  emitChange,
  folders,
  selectedImageIds,
}: TaskDialogOptions): TaskDialogs {
  const { current, settings, saveSettings } = useAppRuntime();
  const { notify } = useNotices();

  const [importOpen, setImportOpen] = useState(false);
  const [pendingPaths, setPendingPaths] = useState<string[]>([]);
  const [similarityGroupsOpen, setSimilarityGroupsOpen] = useState(false);
  const [similarityGroupsTaskId, setSimilarityGroupsTaskId] = useState<string | null>(null);
  const [similarityGroupsContext, setSimilarityGroupsContext] =
    useState<BatchTaskContextInfo | null>(null);
  const [batchContext, setBatchContext] = useState<BatchTaskContextInfo | null>(null);
  const [optimizeContext, setOptimizeContext] = useState<BatchTaskContextInfo | null>(null);
  const [maintenanceContext, setMaintenanceContext] = useState<BatchTaskContextInfo | null>(null);
  const [contextsRevision, setContextsRevision] = useState(0);
  const [optimizeStale, setOptimizeStale] = useState(false);
  const contextsRevisionRef = useRef(0);
  const optimizeQueryRef = useRef(0);
  const importOpenRef = useRef(false);
  importOpenRef.current = importOpen;

  /** 相似组保留规则改了就存，不弹“设置已保存”。 */
  const persistKeepSort = useCallback(
    (next: SortSpec[]) => {
      if (!settings) return;
      void saveSettings(
        { ...settings, ui: { ...settings.ui, similarityKeepSort: next } },
        { silent: true },
      ).catch((cause: unknown) => {
        notify("保存保留规则失败", "error", errorMessage(cause));
      });
    },
    [notify, saveSettings, settings],
  );

  // 优化对话框先打开、后补范围计数：只查总数，不跑分面统计。
  // 放在 `if (!current)` 之前，避免关闭图库时少注册一个 effect。
  useEffect(() => {
    if (!optimizeContext?.query || optimizeContext.viewTotal != null || !statsStore) {
      return;
    }
    const generation = optimizeQueryRef.current + 1;
    optimizeQueryRef.current = generation;
    let disposed = false;
    void statsStore
      .fetch("summary", optimizeContext.query)
      .then(({ total }) => {
        if (disposed || generation !== optimizeQueryRef.current) return;
        setOptimizeContext((previous) =>
          previous && previous.query ? { ...previous, viewTotal: total } : previous,
        );
      })
      .catch(() => {
        // 取不到就保持“—”，不阻塞对话框。
      });
    return () => {
      disposed = true;
    };
  }, [optimizeContext, statsStore]);

  const handleContextsChanged = useCallback(() => {
    setContextsRevision((value) => value + 1);
  }, []);

  // 对话框打开期间数据变化：重取范围数量与选择；估算保留，只标记“执行前会复核”。
  useEffect(() => {
    if (contextsRevision === 0 || !statsStore) return;
    const previousBatch = batchContext;
    const previousOptimize = optimizeContext;
    if (!previousBatch && !previousOptimize) return;
    const generation = contextsRevisionRef.current + 1;
    contextsRevisionRef.current = generation;
    const needsCollections = [previousBatch, previousOptimize].some(
      (context) => context?.collectionId != null,
    );
    void (async () => {
      let nextCollections: CollectionInfo[] | null = null;
      if (needsCollections) {
        try {
          nextCollections = await libraryApi.getCollections();
        } catch {
          nextCollections = null;
        }
      }
      const sources: ContextSources = {
        folders,
        collections: nextCollections,
        selectedIds: selectedImageIds,
        libraryTotal: current?.imageCount ?? null,
        tempTotal: current?.tempCount ?? null,
      };
      const [nextBatch, nextOptimize] = await Promise.all([
        previousBatch ? refreshTaskContext(previousBatch, sources, statsStore) : null,
        previousOptimize
          ? refreshTaskContext(previousOptimize, sources, statsStore)
          : null,
      ]);
      if (generation !== contextsRevisionRef.current) return;
      if (nextBatch) setBatchContext((current) => (current ? nextBatch : current));
      if (nextOptimize) {
        setOptimizeContext((current) => (current ? nextOptimize : current));
        setOptimizeStale(true);
      }
    })();
    // 只在计数版本推进时按打开时的上下文重取，不随上下文对象本身重跑。
  }, [contextsRevision, statsStore]);

  const handleImportFinished = () => {
    // 任务结束路径通常也会发同一条变化；emitChange 在短窗口内去重。
    emitChange({
      kind: "membership",
      source: "import",
      reason: "import",
      clearSelection: true,
      similarity: "clear",
    });
  };

  const openImport = (paths: string[] = []) => {
    if (browserPreview) {
      notify("导入需要 Tauri 桌面窗口", "info", "浏览器预览不会读写真实图库。");
      return;
    }
    setPendingPaths(paths);
    setImportOpen(true);
  };

  const closeImport = () => {
    setImportOpen(false);
    setPendingPaths([]);
  };

  return {
    importOpen,
    importOpenRef,
    pendingPaths,
    setPendingPaths,
    setImportOpen,
    openImport,
    closeImport,
    handleImportFinished,
    similarityGroupsOpen,
    setSimilarityGroupsOpen,
    similarityGroupsTaskId,
    setSimilarityGroupsTaskId,
    similarityGroupsContext,
    setSimilarityGroupsContext,
    batchContext,
    setBatchContext,
    optimizeContext,
    setOptimizeContext,
    optimizeStale,
    setOptimizeStale,
    maintenanceContext,
    setMaintenanceContext,
    persistKeepSort,
    handleContextsChanged,
  };
}
