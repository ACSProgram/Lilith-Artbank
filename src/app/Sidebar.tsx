import type { PointerEvent as ReactPointerEvent } from "react";
import { Database, FolderOpen, Settings } from "lucide-react";
import { Button } from "../shared/components/Button";
import { useAppRuntime } from "./runtimeContext";
import { FolderTree } from "../modules/library/FolderTree";
import type { FolderInfo } from "../modules/library/types";

interface SidebarProps {
  onOpenSettings: () => void;
  /** 只用于“还没有打开图库”时的引导按钮；换库入口在设置里。 */
  onOpenLibrary: () => void;
  folders?: FolderInfo[];
  selectedFolderIds?: number[];
  selectedImageIds?: number[];
  onSelectFolders?: (folderIds: number[]) => void;
  onFoldersChanged?: (folders: FolderInfo[]) => void;
  onDataChanged?: () => void;
  onBeginFolderDrag?: (
    folderId: number,
    label: string,
    event: ReactPointerEvent<HTMLDivElement>,
  ) => void;
}

/** 侧栏：图库身份（名称 / 路径 / 计数）与文件夹树；底部只有设置一个入口。 */
export function Sidebar({
  onOpenSettings,
  onOpenLibrary,
  folders,
  selectedFolderIds = [],
  selectedImageIds = [],
  onSelectFolders,
  onFoldersChanged,
  onDataChanged,
  onBeginFolderDrag,
}: SidebarProps) {
  const { current } = useAppRuntime();
  const browsing = current != null && folders != null;

  return (
    <aside className="sidebar">
      <div className="sidebar-heading">
        <span className="sidebar-eyebrow">LIBRARY</span>
        <h2 className="sidebar-title" title={current?.path}>
          {current?.name ?? "Lilith Artbank"}
        </h2>
        {current ? (
          <>
            <span className="sidebar-library-path" title={current.path}>
              {current.path}
            </span>
            <span className="sidebar-count">
              {current.imageCount.toLocaleString()} 张 · 回收站{" "}
              {current.trashCount.toLocaleString()}
            </span>
          </>
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
          onBeginFolderDrag={onBeginFolderDrag ?? (() => undefined)}
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
              onClick={onOpenLibrary}
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
      </div>
    </aside>
  );
}
