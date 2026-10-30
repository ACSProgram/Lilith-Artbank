import { useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { InspectorPanel } from "../modules/library/Inspector";
import { libraryApi } from "../modules/library/api";
import type { DataChange } from "../modules/library/dataChange";
import type { ImageState } from "../modules/library/types";
import { ImageViewer } from "../modules/viewer/ImageViewer";
import type { PoseData } from "../modules/viewer/types";
import type { ViewerSession } from "../shared/viewerSession";
import { useAppRuntime } from "./runtimeContext";
import { useNotices } from "../shared/notices";
import { errorMessage } from "../shared/errors";

interface ViewerHostProps {
  session: ViewerSession;
  onClose: () => void;
  /** 外部数据变化代次；变化后复核活动图是否仍在当前范围。 */
  revalidateRevision?: number;
  onGenerateDescription?: (imageId: number) => Promise<{ text: string; model: string }>;
  onGeneratePose?: (imageId: number) => Promise<PoseData>;
  /** 关闭查看器并跳到图片所在文件夹；由工作区提供。 */
  onOpenFolder?: (folderId: number) => void;
}

/** 当前图是否仍属于查看器的数据源：临时区不区分回收站，其余按打开时的来源判断。 */
function stillVisible(state: ImageState | undefined, session: ViewerSession): boolean {
  if (!state) return false;
  if (session.inTemp) return true;
  return session.expectTrashed ? state.deletedAt != null : state.deletedAt == null;
}

/** 查看器与图库详情侧栏的编排层；两个模块不互相导入。 */
export function ViewerHost({
  session,
  onClose,
  revalidateRevision = 0,
  onGenerateDescription,
  onGeneratePose,
  onOpenFolder,
}: ViewerHostProps) {
  const { settings, saveSettings, fullscreen, setFullscreen, toggleFullscreen } =
    useAppRuntime();
  const { notify } = useNotices();
  const [activeId, setActiveId] = useState(session.initialId);
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const [version, setVersion] = useState(0);
  const filmstripOpen = settings?.ui.viewerFilmstrip ?? true;
  // 查看器的数据源就是这张列表：它变了（刷新、筛选、集合成员变化）就重新判定归属。
  const items = useSyncExternalStore(session.subscribe, session.getItems, session.getItems);

  const refresh = (change: DataChange) => {
    setVersion((value) => value + 1);
    session.refresh(change);
  };

  /** 关闭查看器时一并退出全屏，否则窗口会停在无标题栏状态。 */
  const close = () => {
    if (fullscreen) void setFullscreen(false);
    onClose();
  };
  const closeRef = useRef(close);
  closeRef.current = close;

  // 外部变化后复核当前图；图片离开当前范围时直接退出，不停在过期内容上。
  useEffect(() => {
    let disposed = false;
    void libraryApi
      .getImageStates([activeId])
      .then((rows) => {
        if (disposed) return;
        if (stillVisible(rows.find((item) => item.id === activeId), session)) return;
        notify(
          "当前图片已离开列表",
          "info",
          session.expectTrashed ? "图片已恢复，或已被删除。" : "图片可能已被移入回收站或删除。",
        );
        closeRef.current();
      })
      .catch(() => {
        // 复核失败不阻断查看；下一次变化或关闭会再查。
      });
    return () => {
      disposed = true;
    };
  }, [activeId, version, revalidateRevision, session, notify]);

  /**
   * 图片离开了当前列表就退出：评分/标签/描述改动可能让它不再符合筛选，集合成员变化
   * 也会让它不在结果里。留在查看器里既看的是过期内容，←/→ 也没有相邻图。
   * 列表为空表示正在换结果集，此时不判定。
   */
  const stillInList = useMemo(
    () => items.length === 0 || items.some((item) => item.id === activeId),
    [activeId, items],
  );
  useEffect(() => {
    if (stillInList) return;
    notify(
      "当前图片已离开列表",
      "info",
      "它可能已被删除或恢复，或不再符合当前筛选与集合。",
    );
    closeRef.current();
  }, [notify, stillInList]);

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
      onGeneratePose={
        onGeneratePose
          ? async () => {
              const pose = await onGeneratePose(activeId);
              refresh({
                kind: "features",
                source: "local",
                reason: "pose",
                ids: [activeId],
                feature: "pose",
                similarity: "clear",
              });
              return pose;
            }
          : undefined
      }
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
          inTrash={session.expectTrashed}
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
          onOpenFolder={
            onOpenFolder
              ? (folderId) => {
                  close();
                  onOpenFolder(folderId);
                }
              : undefined
          }
        />
      }
    />
  );
}
