import { useEffect, useRef, useState } from "react";
import {
  Activity,
  Database,
  FolderOpen,
  Images,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { Button, IconButton } from "../shared/components/Button";
import { ImportDialog } from "../modules/import/ImportDialog";
import { LibraryView } from "../modules/library/LibraryView";
import { browserPreview } from "./api";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";
import { ThumbBenchOverlay } from "./ThumbBenchOverlay";

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "未知错误";
}

const upcoming = [
  ["Stage 3", "标签/评分/描述、搜索、筛选、集合与回收站"],
  ["Stage 4", "查看器、旋转/翻转/裁剪/调尺寸"],
  ["Stage 5", "色彩/LUT/DFT/卷积与操作链撤销"],
];

export function LibraryWorkspace() {
  const { current, refreshStatus, closeLibrary } = useAppRuntime();
  const { notify, confirm } = useNotices();
  const [benchOpen, setBenchOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const [pendingPaths, setPendingPaths] = useState<string[]>([]);
  const [dragging, setDragging] = useState(false);
  const [refreshToken, setRefreshToken] = useState(0);
  const importOpenRef = useRef(false);
  importOpenRef.current = importOpen;

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

  if (!current) return null;

  const hasImages = current.imageCount > 0;

  const handleImportFinished = () => {
    setRefreshToken((value) => value + 1);
    void refreshStatus().catch((error: unknown) => {
      notify("刷新图库状态失败", "error", errorMessage(error));
    });
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
        <LibraryView key={current.id} refreshToken={refreshToken} />
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

      <ThumbBenchOverlay open={benchOpen} onClose={() => setBenchOpen(false)} />
      <ImportDialog
        open={importOpen}
        initialPaths={pendingPaths}
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
  );
}
