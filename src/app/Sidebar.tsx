import type { PointerEvent as ReactPointerEvent } from "react";
import { Database, FolderOpen, Settings } from "lucide-react";
import { Button } from "../shared/components/Button";
import { useNotices } from "../shared/notices";
import { useAppRuntime } from "./runtimeContext";
import { errorMessage } from "../shared/errors";
import { FolderTree } from "../modules/library/FolderTree";
import type { FolderInfo, TrashFolders } from "../modules/library/types";

interface SidebarProps {
  onOpenSettings: () => void;
  /** 只用于“还没有打开图库”时的引导按钮；换库入口在设置里。 */
  onOpenLibrary: () => void;
  libraryId?: string;
  folders?: FolderInfo[];
  selectedFolderIds?: number[];
  directFolderId?: number | null;
  selectedImageIds?: number[];
  onSelectFolders?: (folderIds: number[]) => void;
  onSelectDirect?: (folderId: number) => void;
  onFoldersChanged?: (folders: FolderInfo[]) => void;
  onDataChanged?: (
    delta?: { images?: number; trash?: number },
    reason?: string,
  ) => void;
  /** 图片被移入某个文件夹；带 ids 与目标路径。 */
  onImagesMoved?: (imageIds: number[], targetPath: string) => void;
  /** 回收站视图：左侧显示回收站文件夹树。 */
  trashMode?: boolean;
  trashFolders?: TrashFolders | null;
  trashDirectSelected?: boolean;
  onSelectTrashDirect?: () => void;
  onBeginFolderDrag?: (
    folderIds: number[],
    label: string,
    event: ReactPointerEvent<HTMLDivElement>,
  ) => void;
}

/** 侧栏：图库身份（名称 / 路径 / 计数）与文件夹树；底部只有设置一个入口。 */
export function Sidebar({
  onOpenSettings,
  onOpenLibrary,
  libraryId = "",
  folders,
  selectedFolderIds = [],
  directFolderId = null,
  selectedImageIds = [],
  onSelectFolders,
  onSelectDirect,
  onFoldersChanged,
  onDataChanged,
  onImagesMoved,
  onBeginFolderDrag,
  trashMode = false,
  trashFolders = null,
  trashDirectSelected = false,
  onSelectTrashDirect,
}: SidebarProps) {
  const { current, settings, saveSettings } = useAppRuntime();
  const { notify } = useNotices();
  const browsing = current != null && folders != null;
  const folderSplit = settings?.ui.folderSplit ?? false;
  const folderSplitRatio = settings?.ui.folderSplitRatio ?? 0.5;

  const updateFolderSplit = (update: {
    folderSplit?: boolean;
    folderSplitRatio?: number;
  }) => {
    if (!settings) return;
    void saveSettings(
      { ...settings, ui: { ...settings.ui, ...update } },
      { silent: true },
    ).catch((cause: unknown) => {
      notify("保存界面偏好失败", "error", errorMessage(cause));
    });
  };

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
          key={libraryId}
          libraryId={libraryId}
          folders={trashMode ? (trashFolders?.folders ?? []) : folders}
          selectedFolderIds={selectedFolderIds}
          directFolderId={directFolderId}
          selectedImageIds={selectedImageIds}
          onSelect={onSelectFolders ?? (() => undefined)}
          onSelectDirect={onSelectDirect ?? (() => undefined)}
          onFoldersChanged={onFoldersChanged ?? (() => undefined)}
          onDataChanged={onDataChanged ?? (() => undefined)}
          onImagesMoved={onImagesMoved}
          onBeginFolderDrag={onBeginFolderDrag ?? (() => undefined)}
          mode={trashMode ? "trash" : "normal"}
          trashDirectCount={trashFolders?.directImageCount ?? 0}
          trashTotalCount={trashFolders?.totalImageCount ?? 0}
          trashDirectSelected={trashDirectSelected}
          onSelectTrashDirect={onSelectTrashDirect}
          split={folderSplit}
          splitRatio={folderSplitRatio}
          onToggleSplit={() => updateFolderSplit({ folderSplit: !folderSplit })}
          onSplitRatioChange={(ratio) => updateFolderSplit({ folderSplitRatio: ratio })}
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
