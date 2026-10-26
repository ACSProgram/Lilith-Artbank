import { useState } from "react";
import { InspectorPanel } from "../modules/library/Inspector";
import { ImageViewer } from "../modules/viewer/ImageViewer";
import type { PoseData } from "../modules/viewer/types";
import type { ViewerSession } from "../shared/viewerSession";
import { useAppRuntime } from "./runtimeContext";
import { useNotices } from "./noticeContext";
import { errorMessage } from "../shared/errors";

interface ViewerHostProps {
  session: ViewerSession;
  onClose: () => void;
  onGenerateDescription?: (imageId: number) => Promise<{ text: string; model: string }>;
  onGeneratePose?: (imageId: number) => Promise<PoseData>;
}

/** 查看器与图库详情侧栏的编排层；两个模块不互相导入。 */
export function ViewerHost({
  session,
  onClose,
  onGenerateDescription,
  onGeneratePose,
}: ViewerHostProps) {
  const { settings, saveSettings, fullscreen, setFullscreen, toggleFullscreen } =
    useAppRuntime();
  const { notify } = useNotices();
  const [activeId, setActiveId] = useState(session.initialId);
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const [version, setVersion] = useState(0);
  const filmstripOpen = settings?.ui.viewerFilmstrip ?? true;

  const refresh = () => {
    setVersion((value) => value + 1);
    session.refresh();
  };

  /** 关闭查看器时一并退出全屏，否则窗口会停在无标题栏状态。 */
  const close = () => {
    if (fullscreen) void setFullscreen(false);
    onClose();
  };

  const toggleFilmstrip = () => {
    if (!settings) return;
    void saveSettings(
      { ...settings, ui: { ...settings.ui, viewerFilmstrip: !filmstripOpen } },
      { silent: true },
    ).catch((cause: unknown) => {
      notify("保存界面偏好失败", "error", errorMessage(cause));
    });
  };

  return (
    <ImageViewer
      session={session}
      onClose={close}
      sidebarOpen={sidebarOpen}
      onToggleSidebar={() => setSidebarOpen((value) => !value)}
      onActiveChange={setActiveId}
      onSaved={() => setVersion((value) => value + 1)}
      onGeneratePose={onGeneratePose ? () => onGeneratePose(activeId) : undefined}
      filmstripOpen={filmstripOpen}
      onToggleFilmstrip={toggleFilmstrip}
      fullscreen={fullscreen}
      onToggleFullscreen={() => void toggleFullscreen()}
      sidebar={
        <InspectorPanel
          selectedIds={[activeId]}
          version={version}
          folders={session.folders}
          currentCollectionId={session.currentCollectionId}
          inTemp={session.inTemp}
          onClose={() => setSidebarOpen(false)}
          onChanged={refresh}
          onAddToCollection={() => session.addToCollection([activeId])}
          onRemoveFromCollection={(ids) => void session.removeFromCollection(ids)}
          onTrash={(ids) => {
            void session.trash(ids).then((changed) => {
              if (changed) close();
            });
          }}
          onRestore={(ids) => {
            void session.restore(ids).then((changed) => {
              if (changed) close();
            });
          }}
          onDelete={(ids) => {
            void session.delete(ids).then((changed) => {
              if (changed) close();
            });
          }}
          onFilterTag={(tagId, action, scope) => {
            session.filterByTag(tagId, action, scope);
            close();
          }}
          filterTagIds={session.filterTagIds}
          folderScopeLabel={session.folderScopeLabel}
          collectionScopeLabel={session.collectionScopeLabel}
          onGenerateDescription={onGenerateDescription}
        />
      }
    />
  );
}
