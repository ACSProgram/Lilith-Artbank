import { Database, FolderOpen, Settings, X } from "lucide-react";
import { Button, IconButton } from "../shared/components/Button";
import { useAppRuntime } from "./runtimeContext";
import { FolderTree } from "../modules/library/FolderTree";
import type { FolderInfo } from "../modules/library/types";

interface SidebarProps {
  onOpenSettings: () => void;
  onChangeLibrary: () => void;
  onCloseLibrary: () => void;
  folders?: FolderInfo[];
  selectedFolderIds?: number[];
  selectedImageIds?: number[];
  onSelectFolders?: (folderIds: number[]) => void;
  onFoldersChanged?: (folders: FolderInfo[]) => void;
  onDataChanged?: () => void;
}

export function Sidebar({
  onOpenSettings,
  onChangeLibrary,
  onCloseLibrary,
  folders,
  selectedFolderIds = [],
  selectedImageIds = [],
  onSelectFolders,
  onFoldersChanged,
  onDataChanged,
}: SidebarProps) {
  const { current, appInfo } = useAppRuntime();
  const browsing = current != null && folders != null;

  return (
    <aside className="sidebar">
      <div className="sidebar-heading">
        <span className="sidebar-eyebrow">LIBRARY</span>
        <div className="sidebar-heading-row">
          <h2 title={current?.path}>{current?.name ?? "Lilith Artbank"}</h2>
          {current ? (
            <IconButton label="关闭图库" onClick={onCloseLibrary}>
              <X size={14} />
            </IconButton>
          ) : null}
        </div>
        {current ? (
          <span className="sidebar-library-path" title={current.path}>
            {current.path}
          </span>
        ) : null}
      </div>

      {browsing ? (
        <FolderTree
          folders={folders}
          selectedFolderIds={selectedFolderIds}
          selectedImageIds={selectedImageIds}
          onSelect={onSelectFolders ?? (() => undefined)}
          onFoldersChanged={onFoldersChanged ?? (() => undefined)}
          onDataChanged={onDataChanged ?? (() => undefined)}
        />
      ) : (
        <div className="library-list">
          <div className="sidebar-empty">
            <Database size={20} />
            <span>还没有打开图库</span>
            <Button
              size="sm"
              variant="primary"
              icon={<FolderOpen size={14} />}
              onClick={onChangeLibrary}
            >
              打开或创建
            </Button>
          </div>
        </div>
      )}

      <div className="sidebar-footer">
        <button type="button" className="sidebar-footer-button" onClick={onOpenSettings}>
          <Settings size={16} />
          <span>设置</span>
        </button>
        {current ? (
          <button type="button" className="sidebar-footer-button" onClick={onChangeLibrary}>
            <FolderOpen size={16} />
            <span>更换图库</span>
          </button>
        ) : null}
        <span className="sidebar-version">v{appInfo?.version ?? "—"}</span>
      </div>
    </aside>
  );
}
