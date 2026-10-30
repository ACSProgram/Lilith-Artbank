import { useCallback, useEffect, useMemo, useState } from "react";
import { FolderOpen, Images, Trash2, Upload } from "lucide-react";
import { Button } from "../shared/components/Button";
import { ImportDialog } from "../modules/import/ImportDialog";
import type { ImportTarget } from "../modules/import/types";
import { LibraryView } from "../modules/library/LibraryView";
import { libraryApi } from "../modules/library/api";
import {
  DEFAULT_KEEP_SORT,
  browseToFolder,
  sortForViewChange,
} from "../modules/library/model";
import { useLibraryBrowse } from "../modules/library/useLibraryBrowse";
import { createStatsStore } from "../modules/library/statsStore";
import { StatsStoreProvider } from "../modules/library/StatsProvider";
import { aiApi } from "../modules/ai/api";
import { useAiStatus } from "../modules/ai/useAiStatus";
import { viewerApi } from "../modules/viewer/api";
import { browserPreview } from "./api";
import { useSimilarityBrowse } from "./useSimilarityBrowse";
import { SimilarityGroupsDialog } from "./SimilarityGroupsDialog";
import { BatchTaskDialog } from "./BatchTaskDialog";
import { MaintenanceDialog } from "./MaintenanceDialog";
import { OptimizeDialog } from "./OptimizeDialog";
import { useNotices } from "../shared/notices";
import { useAppRuntime } from "./runtimeContext";
import { Sidebar } from "./Sidebar";
import { ViewerHost } from "./ViewerHost";
import { errorMessage } from "../shared/errors";
import { useLibraryData } from "./useLibraryData";
import { useTaskDialogs } from "./useTaskDialogs";
import { useFolderDrag } from "./useFolderDrag";

interface LibraryWorkspaceProps {
  onOpenSettings: () => void;
  onChangeLibrary: () => void;
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
  const browse = useLibraryBrowse(settings?.ui.sort);
  const similarity = useSimilarityBrowse();
  const { status: aiStatus, refresh: refreshAi } = useAiStatus(Boolean(current));
  const [dragging, setDragging] = useState(false);

  useEffect(() => {
    similarity.clear();
    // 图库切换时丢弃旧库的相似结果；clear 是稳定回调。
  }, [current?.id, similarity.clear]);

  const {
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
  } = useLibraryData(browse);

  const {
    importOpen,
    pendingPaths,
    importOpenRef,
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
  } = useTaskDialogs({ statsStore, emitChange, folders, selectedImageIds });

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

  const { beginDrag } = useFolderDrag({ folders, setFolders, emitChange });

  const importTarget: ImportTarget =
    browse.browse.view === "temp" ? "temp" : "library";
  const importFolder = useMemo(() => {
    if (importTarget === "temp") return "";
    const selected = browse.browse.filters.folderIds;
    if (selected.length !== 1) return "";
    return folders.find((folder) => folder.id === selected[0])?.path ?? "";
  }, [browse.browse.filters.folderIds, folders, importTarget]);

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

  const handleDataChanged = (
    delta?: { images?: number; trash?: number },
    reason?: string,
  ) => {
    if (delta && ((delta.images ?? 0) !== 0 || (delta.trash ?? 0) !== 0)) {
      // 文件夹整棵移入/移出回收站：图片可见性也变了，按回收站变化刷新列表与计数。
      emitChange({
        kind: "membership",
        source: "local",
        reason: reason ?? "folderTrash",
        countDelta: delta,
        clearSelection: true,
        similarity: "clear",
      });
      return;
    }
    // 空文件夹的恢复/永久删除没有图片增量，也要重取正常树与回收站树。
    emitChange({
      kind: "folder",
      source: "local",
      reason: "folderCatalog",
      fields: ["folder"],
      similarity: "keep",
    });
  };

  const selectFolderScope = (folderIds: number[], folderDirect: boolean) => {
    // 相似/语义结果是独立的结果集，切文件夹必须退出它，否则网格与左侧高亮会对不上。
    similarity.clear();
    const leavingTemp = browse.browse.view === "temp";
    browse.applyBrowse({
      ...browse.browse,
      ...(leavingTemp ? sortForViewChange(browse.browse, "all") : {}),
      view: leavingTemp ? "all" : browse.browse.view,
      collectionId: leavingTemp ? null : browse.browse.collectionId,
      filters: { ...browse.browse.filters, folderIds, folderDirect, trashLoose: false },
    });
  };

  const selectFolders = (folderIds: number[]) => {
    selectFolderScope(folderIds, false);
  };

  /** 回收站根行的“直属图片”：原文件夹仍在图库中的回收站图片。 */
  const selectTrashDirect = () => {
    similarity.clear();
    browse.applyBrowse({
      ...browse.browse,
      view: "trash",
      collectionId: null,
      filters: {
        ...browse.browse.filters,
        folderIds: [],
        folderDirect: false,
        trashLoose: true,
      },
    });
  };

  /** 查看器里跳到文件夹：退出回收站/临时视图并清空其它条件；查看器由 ViewerHost 关闭。 */
  const openFolderFromViewer = (folderId: number) => {
    similarity.clear();
    browse.applyBrowse(browseToFolder(browse.browse, folderId));
  };

  const handleCopyImage = async (imageId: number) => {
    try {
      await viewerApi.copyImage(imageId);
      notify("图片已复制到剪贴板", "success");
    } catch (error) {
      notify("复制图片失败", "error", errorMessage(error));
    }
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
        onImagesMoved={(imageIds, targetPath) =>
          emitChange({
            kind: "folder",
            source: "local",
            reason: "moveImages",
            ids: imageIds,
            fields: ["folder"],
            targetFolderPath: targetPath,
            similarity: "keep",
          })
        }
        onBeginFolderDrag={(folderIds, label, event) =>
          beginDrag({ kind: "folders", folderIds, label }, event)
        }
        trashMode={browse.browse.view === "trash"}
        trashFolders={trashFolders}
        trashDirectSelected={browse.browse.filters.trashLoose}
        onSelectTrashDirect={selectTrashDirect}
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
                trashFolders={trashFolders}
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
                onOpenSimilarityGroups={(context) => {
                  setSimilarityGroupsContext(context);
                  setSimilarityGroupsOpen(true);
                }}
                onOpenBatchTasks={setBatchContext}
                onOpenMaintenance={setMaintenanceContext}
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
              onOpenFolder={openFolderFromViewer}
            />
          ) : null}
          <BatchTaskDialog
            open={batchContext !== null}
            onClose={() => setBatchContext(null)}
            context={batchContext}
            status={aiStatus}
            onRefreshStatus={refreshAi}
            libraryId={current.id}
          />
          <MaintenanceDialog
            open={maintenanceContext !== null}
            onClose={() => setMaintenanceContext(null)}
            context={maintenanceContext}
            libraryId={current.id}
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
            context={similarityGroupsContext}
            keepSort={
              settings?.ui.similarityKeepSort?.length
                ? settings.ui.similarityKeepSort
                : DEFAULT_KEEP_SORT
            }
            onKeepSortChange={persistKeepSort}
            onChanged={() =>
              emitChange({
                kind: "collection",
                source: "local",
                reason: "similarityGroups",
                similarity: "keep",
              })
            }
            onMerged={(imageIds) =>
              emitChange({
                kind: "membership",
                source: "local",
                reason: "similarityMerge",
                ids: imageIds,
                similarity: "clear",
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
