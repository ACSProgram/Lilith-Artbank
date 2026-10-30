import { CollectionManagerDialog } from "./CollectionManagerDialog";
import { CollectionPickerDialog } from "./CollectionPickerDialog";
import {
  CombineCollectionsDialog,
  CreateCollectionDialog,
  RenameCollectionDialog,
} from "./CollectionDialogs";
import { TagManagerDialog, type TagChange } from "./TagManagerDialog";
import type { CollectionInfo } from "./types";

interface LibraryDialogsProps {
  createCollectionOpen: boolean;
  createPreset: CollectionInfo | null;
  collections: CollectionInfo[];
  selectionIds: number[];
  viewLabel: string;
  viewCount: number | null;
  resolveViewIds: () => Promise<number[]>;
  resolveCollectionIds: (collectionId: number) => Promise<number[]>;
  onCollectionCreated: (collection: CollectionInfo) => void;
  onCloseCreate: () => void;

  renameTarget: CollectionInfo | null;
  onCollectionRenamed: () => void;
  onCloseRename: () => void;

  combineOpen: boolean;
  onCollectionsCombined: () => void;
  onCloseCombine: () => void;

  collectionManagerOpen: boolean;
  onCloseCollectionManager: () => void;
  onCollectionsChanged: () => void;

  collectionPickerOpen: boolean;
  onCloseCollectionPicker: () => void;
  onCollectionAdded: () => void;

  tagManagerOpen: boolean;
  onCloseTagManager: () => void;
  onTagsChanged: (info?: TagChange) => void;
}

/** LibraryView 的集合与标签对话框；状态与回调由父组件持有。 */
export function LibraryDialogs({
  createCollectionOpen,
  createPreset,
  collections,
  selectionIds,
  viewLabel,
  viewCount,
  resolveViewIds,
  resolveCollectionIds,
  onCollectionCreated,
  onCloseCreate,
  renameTarget,
  onCollectionRenamed,
  onCloseRename,
  combineOpen,
  onCollectionsCombined,
  onCloseCombine,
  collectionManagerOpen,
  onCloseCollectionManager,
  onCollectionsChanged,
  collectionPickerOpen,
  onCloseCollectionPicker,
  onCollectionAdded,
  tagManagerOpen,
  onCloseTagManager,
  onTagsChanged,
}: LibraryDialogsProps) {
  return (
    <>
      <CreateCollectionDialog
        open={createCollectionOpen}
        collections={collections}
        selectionIds={selectionIds}
        viewLabel={viewLabel}
        viewCount={viewCount}
        resolveViewIds={resolveViewIds}
        presetCollection={createPreset}
        resolveCollectionIds={resolveCollectionIds}
        onCreated={onCollectionCreated}
        onClose={onCloseCreate}
      />
      <RenameCollectionDialog
        collection={renameTarget}
        onRenamed={onCollectionRenamed}
        onClose={onCloseRename}
      />
      <CombineCollectionsDialog
        open={combineOpen}
        collections={collections}
        onCombined={onCollectionsCombined}
        onClose={onCloseCombine}
      />
      <CollectionManagerDialog
        open={collectionManagerOpen}
        collections={collections}
        onClose={onCloseCollectionManager}
        onChanged={onCollectionsChanged}
      />
      <CollectionPickerDialog
        open={collectionPickerOpen}
        imageIds={selectionIds}
        collections={collections}
        onClose={onCloseCollectionPicker}
        onAdded={onCollectionAdded}
      />
      <TagManagerDialog
        open={tagManagerOpen}
        onClose={onCloseTagManager}
        onChanged={onTagsChanged}
      />
    </>
  );
}
