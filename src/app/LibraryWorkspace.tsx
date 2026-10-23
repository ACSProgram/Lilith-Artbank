import { Database, FolderOpen, Images, RefreshCw, Trash2, X } from "lucide-react";
import { Button, IconButton } from "../shared/components/Button";
import { browserPreview } from "./api";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "未知错误";
}

const upcoming = [
  ["Stage 1", "导入、内容哈希、pHash、主色与缩略图包"],
  ["Stage 2", "虚拟化网格、列表视图、选择与键盘操作"],
  ["Stage 3", "标签/评分/描述、筛选、排序、集合与回收站"],
];

export function LibraryWorkspace() {
  const { current, refreshStatus, closeLibrary } = useAppRuntime();
  const { notify, confirm } = useNotices();

  if (!current) return null;

  const handleRefresh = async () => {
    try {
      await refreshStatus();
      notify("图库状态已刷新", "success");
    } catch (error) {
      notify("刷新状态失败", "error", errorMessage(error));
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

  return (
    <div className="workspace">
      <header className="workspace-toolbar">
        <div className="workspace-heading">
          <span className="workspace-eyebrow">LIBRARY</span>
          <div className="workspace-title-row">
            <h1>{current.name}</h1>
            <span className="workspace-path" title={current.path}>
              {current.path}
            </span>
          </div>
        </div>
        <div className="workspace-actions">
          <Button size="sm" variant="ghost" icon={<RefreshCw size={14} />} onClick={() => void handleRefresh()}>
            刷新状态
          </Button>
          <IconButton label="关闭图库" onClick={() => void handleClose()}>
            <X size={16} />
          </IconButton>
        </div>
      </header>

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
        <div className="stat-card stat-card-path">
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
        <h2>图库已就绪</h2>
        <p>
          数据库、清单与目录结构已经建立。下一阶段接入导入管线与缩略图包后，这里会出现虚拟化网格。
        </p>
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
            浏览器预览模式不会创建真实数据库；请用 <code>npm run tauri dev</code> 验证图库读写。
          </p>
        ) : null}
      </div>
    </div>
  );
}
