import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { FolderOpen, Images, Trash2, Upload } from "lucide-react";
import { Button } from "../shared/components/Button";
import { ImportDialog } from "../modules/import/ImportDialog";
import type { ImportTarget } from "../modules/import/types";
import { LibraryView } from "../modules/library/LibraryView";
import { libraryApi } from "../modules/library/api";
import type { CollectionInfo, FolderChange, FolderInfo } from "../modules/library/types";
import { sortForViewChange } from "../modules/library/model";
import type { DataChange, DataChangeSignal } from "../modules/library/dataChange";
import { useLibraryBrowse } from "../modules/library/useLibraryBrowse";
import {
  createStatsStore,
  type StatsStore,
} from "../modules/library/statsStore";
import { StatsStoreProvider } from "../modules/library/StatsProvider";
import { aiApi } from "../modules/ai/api";
import { useAiStatus } from "../modules/ai/useAiStatus";
import { viewerApi } from "../modules/viewer/api";
import { browserPreview } from "./api";
import { useSimilarityBrowse } from "./useSimilarityBrowse";
import { SimilarityGroupsDialog } from "./SimilarityGroupsDialog";
import { BatchTaskDialog } from "./BatchTaskDialog";
import { OptimizeDialog } from "./OptimizeDialog";
import type { BatchTaskContextInfo } from "../shared/batchTaskContext";
import { useTaskData } from "./taskContext";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";
import { Sidebar } from "./Sidebar";
import { ViewerHost } from "./ViewerHost";
import { errorMessage } from "../shared/errors";
import type { ViewerSession } from "../shared/viewerSession";
import { useDragSort, type DragPayload, type DropTarget } from "../shared/useDragSort";
import {
  applyFolderChange,
  foldersById,
  isSameOrDescendant,
  outermostFolders,
  parentFolderPath,
} from "../modules/library/folderTreeModel";

interface LibraryWorkspaceProps {
  onOpenSettings: () => void;
  onChangeLibrary: () => void;
}

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

export function LibraryWorkspace({ onOpenSettings, onChangeLibrary }: LibraryWorkspaceProps) {
  const { current, settings } = useAppRuntime();
  const { notify } = useNotices();
  const statsStore = useMemo(
    () =>
      current?.id
        ? createStatsStore({ fetch: (request) => libraryApi.getLibraryStats(request) })
        : null,
    [current?.id],
  );
  useEffect(() => {
    if (!statsStore) return;
    return () => statsStore.dispose();
  }, [statsStore]);
  const [importOpen, setImportOpen] = useState(false);
  const [pendingPaths, setPendingPaths] = useState<string[]>([]);
  const [dragging, setDragging] = useState(false);
  const [changeSignal, setChangeSignal] = useState<DataChangeSignal | null>(null);
  const [viewerSession, setViewerSession] = useState<ViewerSession | null>(null);
  const [folders, setFolders] = useState<FolderInfo[]>([]);
  const [foldersLoaded, setFoldersLoaded] = useState(false);
  const [selectedImageIds, setSelectedImageIds] = useState<number[]>([]);
  const [similarityGroupsOpen, setSimilarityGroupsOpen] = useState(false);
  const [similarityGroupsTaskId, setSimilarityGroupsTaskId] = useState<string | null>(null);
  const [batchContext, setBatchContext] = useState<BatchTaskContextInfo | null>(null);
  const [optimizeContext, setOptimizeContext] = useState<BatchTaskContextInfo | null>(null);
  const [contextsRevision, setContextsRevision] = useState(0);
  const [optimizeStale, setOptimizeStale] = useState(false);
  const contextsRevisionRef = useRef(0);
  const browse = useLibraryBrowse(settings?.ui.sort);
  const importTarget: ImportTarget =
    browse.browse.view === "temp" ? "temp" : "library";
  const importFolder = useMemo(() => {
    if (importTarget === "temp") return "";
    const selected = browse.browse.filters.folderIds;
    if (selected.length !== 1) return "";
    return folders.find((folder) => folder.id === selected[0])?.path ?? "";
  }, [browse.browse.filters.folderIds, folders, importTarget]);
  const similarity = useSimilarityBrowse();
  const { lastFinished, lastChange } = useTaskData();
  const { status: aiStatus, refresh: refreshAi } = useAiStatus(Boolean(current));
  const optimizeQueryRef = useRef(0);
  const importOpenRef = useRef(false);
  importOpenRef.current = importOpen;
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

  useEffect(() => {
    similarity.clear();
    // 图库切换时丢弃旧库的相似结果；clear 是稳定回调。
  }, [current?.id, similarity.clear]);

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
    void reloadFolders();
  }, [reloadFolders, current?.id]);

  useEffect(() => {
    if (!foldersLoaded) return;
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

  useEffect(() => {
    if (browserPreview) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void (async () => {
      const { getCurrentWebview } = await import("@tauri-apps/api/webview");
      if (disposed) return;
      unlisten = await getCurrentWebview().onDragDropEvent((event) => {
        const type = event.payload.type;
        if (type === "enter" || type === "over") {
          setDragging(true);
        } else if (type === "leave") {
          setDragging(false);
        } else if (type === "drop") {
          setDragging(false);
          if (!importOpenRef.current && event.payload.paths.length > 0) {
            setPendingPaths(event.payload.paths);
            setImportOpen(true);
          }
        }
      });
    })();
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

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

  const generateDescription = useCallback(async (imageId: number) => {
    const result = await aiApi.describeImage(imageId);
    return { text: result.text, model: result.model };
  }, []);

  const searchTagSuggestions = useCallback(
    (query: string) =>
      libraryApi
        .searchTags(query, 200)
        .then((tags) => tags.map(({ name, imageCount }) => ({ name, imageCount }))),
    [],
  );

  const generatePose = useCallback(async (imageId: number) => {
    return aiApi.generatePose(imageId);
  }, []);

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

  if (!current) return null;

  const hasImages =
    current.imageCount > 0 || current.trashCount > 0 || current.tempCount > 0;
  const mean = aiStatus?.mean;
  const semanticBlockedReason = !aiStatus?.online
    ? "AI 服务离线，语义搜索需要文本模型"
    : mean?.phase === "failed"
      ? (mean.message ?? "embedding 均值计算失败，请重新打开图库")
      : mean?.phase === "missing" && mean.count === 0
        ? (mean.message ?? "还没有图片 embedding，可在批量任务里生成")
        : null;

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

  const handleDataChanged = () => {
    // FolderTree 已经通过 onFoldersChanged 送回新的计数，只需刷新图片路径相关结果。
    emitChange({
      kind: "folder",
      source: "local",
      reason: "folderCatalog",
      fields: ["folder"],
      foldersAlreadyLoaded: true,
      similarity: "keep",
    });
  };

  const selectFolderScope = (folderIds: number[], folderDirect: boolean) => {
    const leavingTemp = browse.browse.view === "temp";
    browse.applyBrowse({
      ...browse.browse,
      ...(leavingTemp ? sortForViewChange(browse.browse, "all") : {}),
      view: leavingTemp ? "all" : browse.browse.view,
      collectionId: leavingTemp ? null : browse.browse.collectionId,
      filters: { ...browse.browse.filters, folderIds, folderDirect },
    });
  };

  const selectFolders = (folderIds: number[]) => {
    selectFolderScope(folderIds, false);
  };

  const handleCopyImage = async (imageId: number) => {
    try {
      await viewerApi.copyImage(imageId);
      notify("图片已复制到剪贴板", "success");
    } catch (error) {
      notify("复制图片失败", "error", errorMessage(error));
    }
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

  return (
    <div className="app-body">
      <Sidebar
        onOpenSettings={onOpenSettings}
        onOpenLibrary={onChangeLibrary}
        libraryId={current.id}
        folders={folders}
        selectedFolderIds={browse.browse.filters.folderIds}
        directFolderId={
          browse.browse.filters.folderDirect && browse.browse.filters.folderIds.length === 1
            ? browse.browse.filters.folderIds[0]
            : null
        }
        selectedImageIds={selectedImageIds}
        onSelectFolders={selectFolders}
        onSelectDirect={(folderId) => selectFolderScope([folderId], true)}
        onFoldersChanged={setFolders}
        onDataChanged={handleDataChanged}
        onBeginFolderDrag={(folderIds, label, event) =>
          beginDrag({ kind: "folders", folderIds, label }, event)
        }
      />
      <main className="app-main">
        <div className={`workspace${hasImages ? " workspace-browse" : ""}`}>
          {hasImages && statsStore ? (
            <StatsStoreProvider store={statsStore}>
              <LibraryView
                key={current.id}
                browse={browse.browse}
                canGoBack={browse.canGoBack}
                folders={folders}
                onApplyBrowse={browse.applyBrowse}
                onGoBack={browse.goBack}
                onFoldersChanged={() => reloadFolders()}
                onImageSelectionChange={setSelectedImageIds}
                changeSignal={changeSignal}
                onOpenViewer={setViewerSession}
                onCloseViewer={() => setViewerSession(null)}
                onContextsChanged={handleContextsChanged}
                onCopyImage={(imageId) => void handleCopyImage(imageId)}
                onOpenImport={() => openImport()}
                onBeginImageDrag={(imageIds, label, event) =>
                  beginDrag({ kind: "images", imageIds, label }, event)
                }
                similarity={similarity}
                onOpenSimilarityGroups={() => {
                  setSimilarityGroupsOpen(true);
                }}
                onOpenBatchTasks={setBatchContext}
                onOpenOptimize={(context) => {
                  setOptimizeContext(context);
                  setOptimizeStale(false);
                }}
                onGenerateDescription={generateDescription}
                semanticBlockedReason={semanticBlockedReason}
              />
            </StatsStoreProvider>
          ) : hasImages ? null : (
            <>
              <div className="stat-grid">
                <div className="stat-card">
                  <span className="stat-icon"><Images size={18} /></span>
                  <div>
                    <span className="stat-value">{current.imageCount.toLocaleString()}</span>
                    <span className="stat-label">图片</span>
                  </div>
                </div>
                <div className="stat-card">
                  <span className="stat-icon"><Trash2 size={18} /></span>
                  <div>
                    <span className="stat-value">{current.trashCount.toLocaleString()}</span>
                    <span className="stat-label">回收站</span>
                  </div>
                </div>
                <div className="stat-card">
                  <span className="stat-icon"><FolderOpen size={18} /></span>
                  <div>
                    <span className="stat-value stat-value-small" title={current.path}>{current.path}</span>
                    <span className="stat-label">图库位置</span>
                  </div>
                </div>
              </div>

              <div className="workspace-empty">
                <div className="workspace-empty-icon">
                  <Images size={34} />
                </div>
                <h2>把图片拖进来</h2>
                <p>
                  把图片或文件夹拖进窗口，或者从本地选择。原图会复制入库，源文件不受影响。
                </p>
                <div className="workspace-empty-actions">
                  <Button variant="primary" icon={<Upload size={15} />} onClick={() => openImport()}>
                    导入图片
                  </Button>
                </div>
                {import.meta.env.DEV && browserPreview ? (
                  <p className="workspace-note">
                    浏览器预览模式不会创建真实数据库；请用 <code>npm run tauri dev</code> 验证导入与图库读写。
                  </p>
                ) : null}
              </div>
            </>
          )}
        </div>

        <div className="workspace-host">
          {viewerSession ? (
            <ViewerHost
              key={`${current.id}-${viewerSession.initialId}`}
              session={viewerSession}
              onClose={() => setViewerSession(null)}
              revalidateRevision={changeSignal?.revision ?? 0}
              onGenerateDescription={generateDescription}
              onGeneratePose={generatePose}
            />
          ) : null}
          <BatchTaskDialog
            open={batchContext !== null}
            onClose={() => setBatchContext(null)}
            context={batchContext}
            status={aiStatus}
            onRefreshStatus={refreshAi}
          />
          <OptimizeDialog
            open={optimizeContext !== null}
            onClose={() => setOptimizeContext(null)}
            context={optimizeContext}
            libraryId={current.id}
            dataChanged={optimizeStale}
          />
          <ImportDialog
            open={importOpen}
            initialPaths={pendingPaths}
            initialFolder={importFolder}
            target={importTarget}
            folders={folders}
            searchTags={searchTagSuggestions}
            onClose={closeImport}
            onFinished={handleImportFinished}
          />
          <SimilarityGroupsDialog
            open={similarityGroupsOpen}
            onClose={() => {
              setSimilarityGroupsOpen(false);
            }}
            onChanged={() =>
              emitChange({
                kind: "collection",
                source: "local",
                reason: "similarityGroups",
                similarity: "keep",
              })
            }
            initialTaskId={similarityGroupsTaskId}
            onTaskStarted={setSimilarityGroupsTaskId}
          />

          {dragging && !importOpen ? (
            <div className="drop-overlay">
              <div className="drop-overlay-card">
                <Upload size={30} />
                <strong>松开即可导入</strong>
                <span>
                  {importTarget === "temp"
                    ? "图片会被复制到临时文件夹，关闭图库时清空。"
                    : "图片会被复制到当前图库"}
                </span>
              </div>
            </div>
          ) : null}
        </div>
      </main>
    </div>
  );
}
