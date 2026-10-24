import { useCallback, useEffect, useRef, useState } from "react";
import {
  Activity,
  Database,
  FolderOpen,
  Images,
  Sparkles,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { Button, IconButton } from "../shared/components/Button";
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
import { useSemanticSearch } from "./useSemanticSearch";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";
import { Sidebar } from "./Sidebar";
import { ThumbBenchOverlay } from "./ThumbBenchOverlay";
import { ViewerHost } from "./ViewerHost";
import { errorMessage } from "../shared/errors";
import type { ViewerSession } from "../shared/viewerSession";

const upcoming = [["Stage 7", "相似度检索与高级集合"]];

interface LibraryWorkspaceProps {
  onOpenSettings: () => void;
  onChangeLibrary: () => void;
}

export function LibraryWorkspace({ onOpenSettings, onChangeLibrary }: LibraryWorkspaceProps) {
  const { current, refreshStatus, closeLibrary } = useAppRuntime();
  const { notify, confirm } = useNotices();
  const [benchOpen, setBenchOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const [pendingPaths, setPendingPaths] = useState<string[]>([]);
  const [dragging, setDragging] = useState(false);
  const [refreshToken, setRefreshToken] = useState(0);
  const [viewerSession, setViewerSession] = useState<ViewerSession | null>(null);
  const [folders, setFolders] = useState<FolderInfo[]>([]);
  const [foldersLoaded, setFoldersLoaded] = useState(false);
  const [selectedImageIds, setSelectedImageIds] = useState<number[]>([]);
  const [aiTasks, setAiTasks] = useState<{
    selectedIds: number[];
    query: LibraryQuery | null;
  } | null>(null);
  const browse = useLibraryBrowse();
  const semantic = useSemanticSearch();
  const { status: aiStatus, refresh: refreshAi } = useAiStatus(Boolean(current));
  const importOpenRef = useRef(false);
  importOpenRef.current = importOpen;

  useEffect(() => {
    semantic.clear();
    // 图库切换时丢弃旧库的语义结果；clear 是稳定回调。
  }, [current?.id, semantic.clear]);

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

  const handleClose = async () => {
    const accepted = await confirm({
      title: "关闭图库？",
      message: "关闭只释放数据库连接，不会修改图库内容。",
      confirmLabel: "关闭",
    });
    if (!accepted) return;
    try {
      await closeLibrary();
    } catch (error) {
      notify("关闭图库失败", "error", errorMessage(error));
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
        onChangeLibrary={onChangeLibrary}
        onCloseLibrary={() => void handleClose()}
        folders={folders}
        selectedFolderIds={browse.browse.filters.folderIds}
        selectedImageIds={selectedImageIds}
        onSelectFolders={selectFolders}
        onFoldersChanged={setFolders}
        onDataChanged={handleDataChanged}
      />
      <main className="app-main">
        <div className={`workspace${hasImages ? " workspace-browse" : ""}`}>
          <header className="workspace-toolbar">
            <div className="workspace-heading">
              <span className="workspace-eyebrow">LIBRARY</span>
              <div className="workspace-title-row">
                <h1>{current.name}</h1>
                <span className="workspace-count">
                  {current.imageCount.toLocaleString()} 张 · 回收站{" "}
                  {current.trashCount.toLocaleString()}
                </span>
                <span className="workspace-path" title={current.path}>
                  {current.path}
                </span>
              </div>
            </div>
            <div className="workspace-actions">
              <button
                type="button"
                className={`ai-status-pill${aiStatus?.online ? " is-online" : ""}`}
                title={aiStatus?.process.message ?? "打开 AI 任务"}
                onClick={() => setAiTasks({ selectedIds: [], query: null })}
              >
                <Sparkles size={13} />
                {aiStatus?.online
                  ? "AI 在线"
                  : aiStatus?.process.state === "starting"
                    ? "AI 启动中"
                    : "AI 离线"}
              </button>
              {import.meta.env.DEV && !browserPreview ? (
                <Button
                  size="sm"
                  variant="ghost"
                  icon={<Activity size={14} />}
                  onClick={() => setBenchOpen(true)}
                >
                  缩略图基准
                </Button>
              ) : null}
              <Button
                size="sm"
                variant="primary"
                icon={<Upload size={14} />}
                onClick={() => openImport()}
              >
                导入图片
              </Button>
              <IconButton label="关闭图库" onClick={() => void handleClose()}>
                <X size={16} />
              </IconButton>
            </div>
          </header>

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
              semantic={semantic}
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
                  <span className="stat-icon"><Database size={18} /></span>
                  <div>
                    <span className="stat-value">v{current.schemaVersion}</span>
                    <span className="stat-label">Schema</span>
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
                  把图片或文件夹拖进窗口，或者从本地选择。原图复制入库，缩略图、pHash 与主色同步生成。
                </p>
                <div className="workspace-empty-actions">
                  <Button variant="primary" icon={<Upload size={15} />} onClick={() => openImport()}>
                    导入图片
                  </Button>
                </div>
                <div className="stage-list">
                  {upcoming.map(([stage, text]) => (
                    <div key={stage} className="stage-item">
                      <span className="stage-badge">{stage}</span>
                      <span>{text}</span>
                    </div>
                  ))}
                </div>
                {browserPreview ? (
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
