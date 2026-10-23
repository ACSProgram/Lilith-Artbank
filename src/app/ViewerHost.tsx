import { useState } from "react";
import { InspectorPanel } from "../modules/library/Inspector";
import { ImageViewer } from "../modules/viewer/ImageViewer";
import type { PoseData } from "../modules/viewer/types";
import type { ViewerSession } from "../shared/viewerSession";

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
  const [activeId, setActiveId] = useState(session.initialId);
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const [version, setVersion] = useState(0);

  const refresh = () => {
    setVersion((value) => value + 1);
    session.refresh();
  };

  return (
    <ImageViewer
      session={session}
      onClose={onClose}
      sidebarOpen={sidebarOpen}
      onToggleSidebar={() => setSidebarOpen((value) => !value)}
      onActiveChange={setActiveId}
      onSaved={() => setVersion((value) => value + 1)}
      onGeneratePose={onGeneratePose ? () => onGeneratePose(activeId) : undefined}
      sidebar={
        <InspectorPanel
          selectedIds={[activeId]}
          version={version}
          folders={session.folders}
          currentCollectionId={session.currentCollectionId}
          onClose={() => setSidebarOpen(false)}
          onChanged={refresh}
          onAddToCollection={() => session.addToCollection([activeId])}
          onRemoveFromCollection={(ids) => void session.removeFromCollection(ids)}
          onTrash={(ids) => {
            void session.trash(ids).then((changed) => {
              if (changed) onClose();
            });
          }}
          onRestore={(ids) => {
            void session.restore(ids).then((changed) => {
              if (changed) onClose();
            });
          }}
          onDelete={(ids) => {
            void session.delete(ids).then((changed) => {
              if (changed) onClose();
            });
          }}
          onGenerateDescription={onGenerateDescription}
        />
      }
    />
  );
}
