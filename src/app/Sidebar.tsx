import { Database, FolderOpen, Plus, Settings, Trash2 } from "lucide-react";
import { IconButton } from "../shared/components/Button";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "未知错误";
}

export function Sidebar({
  onAddLibrary,
  onOpenSettings,
}: {
  onAddLibrary: () => void;
  onOpenSettings: () => void;
}) {
  const { settings, current, appInfo, openLibrary, removeLibrary } = useAppRuntime();
  const { notify, confirm } = useNotices();
  const libraries = settings?.libraries ?? [];

  const handleOpen = async (libraryId: string) => {
    if (current?.id === libraryId) return;
    try {
      await openLibrary(libraryId);
    } catch (error) {
      notify("打开图库失败", "error", errorMessage(error));
    }
  };

  const handleRemove = async (libraryId: string, name: string) => {
    const accepted = await confirm({
      title: "从列表移除图库？",
      message: `只会移除「${name}」的列表项，不会删除磁盘上的任何图片或数据。`,
      confirmLabel: "移除",
      danger: true,
    });
    if (!accepted) return;
    try {
      await removeLibrary(libraryId);
      notify("已从列表移除图库", "success");
    } catch (error) {
      notify("移除图库失败", "error", errorMessage(error));
    }
  };

  return (
    <aside className="sidebar">
      <div className="sidebar-heading">
        <span className="sidebar-eyebrow">LIBRARY</span>
        <div className="sidebar-heading-row">
          <h2>图库</h2>
          <IconButton label="添加图库" onClick={onAddLibrary}>
            <Plus size={16} />
          </IconButton>
        </div>
      </div>

      <div className="library-list">
        {libraries.length === 0 ? (
          <div className="sidebar-empty">
            <Database size={20} />
            <span>还没有图库</span>
            <button type="button" className="text-button" onClick={onAddLibrary}>
              打开或创建
            </button>
          </div>
        ) : (
          libraries.map((library) => {
            const active = current?.id === library.id;
            return (
              <div
                key={library.id}
                className={`library-item${active ? " is-active" : ""}`}
              >
                <button
                  type="button"
                  className="library-item-main"
                  onClick={() => void handleOpen(library.id)}
                  title={library.path}
                >
                  <span className="library-item-icon">
                    <FolderOpen size={17} />
                  </span>
                  <span className="library-item-text">
                    <span className="library-item-name">{library.name}</span>
                    <span className="library-item-path">{library.path}</span>
                  </span>
                </button>
                <IconButton
                  label="从列表移除"
                  variant="danger"
                  className="library-item-remove"
                  onClick={() => void handleRemove(library.id, library.name)}
                >
                  <Trash2 size={14} />
                </IconButton>
              </div>
            );
          })
        )}
      </div>

      <div className="sidebar-footer">
        <button type="button" className="sidebar-footer-button" onClick={onOpenSettings}>
          <Settings size={16} />
          <span>设置</span>
        </button>
        <span className="sidebar-version">v{appInfo?.version ?? "—"}</span>
      </div>
    </aside>
  );
}
