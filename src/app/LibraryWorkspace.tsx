import { useCallback, useEffect, useRef, useState } from "react";
import { FolderOpen, Images, Trash2, Upload } from "lucide-react";
import { Button } from "../shared/components/Button";
import { ImportDialog } from "../modules/import/ImportDialog";
import { LibraryView } from "../modules/library/LibraryView";
import { libraryApi } from "../modules/library/api";
import type { FolderInfo, LibraryQuery } from "../modules/library/types";
import { useLibraryBrowse } from "../modules/library/useLibraryBrowse";
import { aiApi } from "../modules/ai/api";
import { AiTaskDialog } from "../modules/ai/AiTaskDialog";
import { useAiStatus } from "../modules/ai/useAiStatus";
import { viewerApi } from "../modules/viewer/api";
import { browserPreview } from "./api";
import { useSimilarityBrowse } from "./useSimilarityBrowse";
import { SimilarityGroupsDialog } from "./SimilarityGroupsDialog";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";
import { Sidebar } from "./Sidebar";
import { ThumbBenchOverlay } from "./ThumbBenchOverlay";
import { ViewerHost } from "./ViewerHost";
import { errorMessage } from "../shared/errors";
import type { ViewerSession } from "../shared/viewerSession";

interface LibraryWorkspaceProps {
  onOpenSettings: () => void;
  onChangeLibrary: () => void;
}

export function LibraryWorkspace({ onOpenSettings, onChangeLibrary }: LibraryWorkspaceProps) {
  const { current, refreshStatus } = useAppRuntime();
  const { notify } = useNotices();
  const [benchOpen, setBenchOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const [pendingPaths, setPendingPaths] = useState<string[]>([]);
  const [dragging, setDragging] = useState(false);
  const [refreshToken, setRefreshToken] = useState(0);
  const [viewerSession, setViewerSession] = useState<ViewerSession | null>(null);
  const [folders, setFolders] = useState<FolderInfo[]>([]);
  const [foldersLoaded, setFoldersLoaded] = useState(false);
  const [selectedImageIds, setSelectedImageIds] = useState<number[]>([]);
  const [similarityGroupsOpen, setSimilarityGroupsOpen] = useState(false);
  const [aiTasks, setAiTasks] = useState<{
    selectedIds: number[];
    query: LibraryQuery | null;
  } | null>(null);
  const browse = useLibraryBrowse();
  const similarity = useSimilarityBrowse();
  const { status: aiStatus, refresh: refreshAi } = useAiStatus(Boolean(current));
  const importOpenRef = useRef(false);
  importOpenRef.current = importOpen;

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
  }, [reloadFolders, refreshToken]);

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
          filters: { ...browse.browse.filters, folderIds: next },
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

  const handleAiFinished = useCallback(() => {
    setRefreshToken((value) => value + 1);
    void refreshStatus().catch((error: unknown) => {
      notify("刷新图库状态失败", "error", errorMessage(error));
    });
    void refreshAi();
  }, [notify, refreshAi, refreshStatus]);

  const generateDescription = useCallback(async (imageId: number) => {
    const result = await aiApi.describeImage(imageId);
    return { text: result.text, model: result.model };
  }, []);

  const generatePose = useCallback(async (imageId: number) => {
    return aiApi.generatePose(imageId);
  }, []);

  if (!current) return null;

  const hasImages = current.imageCount > 0 || current.trashCount > 0;

  const handleImportFinished = () => {
    setRefreshToken((value) => value + 1);
    void reloadFolders();
    void refreshStatus().catch((error: unknown) => {
      notify("刷新图库状态失败", "error", errorMessage(error));
    });
  };

  const handleDataChanged = () => {
    setRefreshToken((value) => value + 1);
  };

  const selectFolders = (folderIds: number[]) => {
    browse.applyBrowse({
      ...browse.browse,
      filters: { ...browse.browse.filters, folderIds },
    });
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
        folders={folders}
        selectedFolderIds={browse.browse.filters.folderIds}
        selectedImageIds={selectedImageIds}
        onSelectFolders={selectFolders}
        onFoldersChanged={setFolders}
        onDataChanged={handleDataChanged}
      />
      <main className="app-main">
        <div className={`workspace${hasImages ? " workspace-browse" : ""}`}>
          {hasImages ? (
            <LibraryView
              key={current.id}
              browse={browse.browse}
              canGoBack={browse.canGoBack}
              folders={folders}
              onApplyBrowse={browse.applyBrowse}
              onGoBack={browse.goBack}
              onFoldersChanged={() => void reloadFolders()}
              onImageSelectionChange={setSelectedImageIds}
              refreshToken={refreshToken}
              onOpenViewer={setViewerSession}
              onCopyImage={(imageId) => void handleCopyImage(imageId)}
              onOpenImport={() => openImport()}
              onOpenBench={import.meta.env.DEV ? () => setBenchOpen(true) : undefined}
              similarity={similarity}
              onFindSimilar={(kind, imageId) => similarity.findSimilar(kind, imageId)}
              onOpenSimilarityGroups={() => setSimilarityGroupsOpen(true)}
              onOpenAiTasks={setAiTasks}
              onGenerateDescription={generateDescription}
              aiOnline={aiStatus?.online ?? false}
            />
          ) : (
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

        <ThumbBenchOverlay open={benchOpen} onClose={() => setBenchOpen(false)} />
        <div className="workspace-host">
          {viewerSession ? (
            <ViewerHost
              key={`${current.id}-${viewerSession.initialId}`}
              session={viewerSession}
              onClose={() => setViewerSession(null)}
              onGenerateDescription={generateDescription}
              onGeneratePose={generatePose}
            />
          ) : null}
          <AiTaskDialog
            open={aiTasks !== null}
            onClose={() => setAiTasks(null)}
            selectedIds={aiTasks?.selectedIds ?? []}
            query={aiTasks?.query ?? null}
            status={aiStatus}
            onRefreshStatus={refreshAi}
            onFinished={handleAiFinished}
          />
          <ImportDialog
            open={importOpen}
            initialPaths={pendingPaths}
            folders={folders}
            onClose={closeImport}
            onFinished={handleImportFinished}
          />
          <SimilarityGroupsDialog
            open={similarityGroupsOpen}
            onClose={() => setSimilarityGroupsOpen(false)}
            onChanged={() => setRefreshToken((value) => value + 1)}
          />

          {dragging && !importOpen ? (
            <div className="drop-overlay">
              <div className="drop-overlay-card">
                <Upload size={30} />
                <strong>松开即可导入</strong>
                <span>图片会被复制到当前图库</span>
              </div>
            </div>
          ) : null}
        </div>
      </main>
    </div>
  );
}
