import { invoke } from "@tauri-apps/api/core";
import type { TaskStartResult } from "../../app/taskTypes";
import {
  mockAddTags,
  mockApplyFolderOrder,
  mockAddToCollection,
  mockClearOperationHistory,
  mockCollections,
  mockCombineCollections,
  mockCreateCollection,
  mockCreateCollectionsFromGroups,
  mockCreateFolder,
  mockDeleteCollection,
  mockDeleteFolder,
  mockDeleteFolderPermanently,
  mockDeleteImages,
  mockDeleteTag,
  mockEmptyTrash,
  mockFolders,
  mockImageDetail,
  mockImageStates,
  mockLibraryCounts,
  mockLibraryStats,
  mockMergeTags,
  mockMoveFolder,
  mockMoveImages,
  mockPlaceFolders,
  mockOperationHistory,
  mockQueryIds,
  mockQueryPage,
  mockRedoToOperation,
  mockReleasePendingFiles,
  mockRemoveFromCollection,
  mockRemoveTags,
  mockRenameCollection,
  mockRenameFolder,
  mockRenameTag,
  mockReorderCollections,
  mockReorderFolders,
  mockRestoreFolder,
  mockRestoreImages,
  mockRetryUndo,
  mockSearchTags,
  mockSelectionTags,
  mockSkipUndo,
  mockSetImageTags,
  mockSetScores,
  mockTags,
  mockTrashFolders,
  mockTrashImages,
  mockUndoToOperation,
  mockUpdateDescription,
  mockUpdateScore,
} from "./mock";
import type {
  CollectionInfo,
  CombineMode,
  DeleteReport,
  FeatureSummary,
  FolderChange,
  FolderInfo,
  FolderOrder,
  FolderSortKey,
  HistoryPage,
  ImageCapabilities,
  ImageDetails,
  ImagePage,
  ImageState,
  LibraryQuery,
  MoveReport,
  SelectionTags,
  SortSpec,
  StatsRequest,
  StatsSnapshot,
  TagInfo,
  TagMergeReport,
  TagRef,
  TrashFolders,
} from "./types";

const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export const libraryApi = {
  async queryImages(query: LibraryQuery): Promise<ImagePage> {
    if (!isTauri()) return mockQueryPage(query);
    return invoke<ImagePage>("query_images", { query });
  },

  async queryImageIds(query: LibraryQuery): Promise<number[]> {
    if (!isTauri()) return mockQueryIds(query);
    return invoke<number[]>("query_image_ids", { query });
  },

  async getLibraryStats(request: StatsRequest): Promise<StatsSnapshot> {
    if (!isTauri()) return mockLibraryStats(request);
    return invoke<StatsSnapshot>("get_library_stats", { request });
  },

  async getFolders(): Promise<FolderInfo[]> {
    if (!isTauri()) return mockFolders();
    return invoke<FolderInfo[]>("get_folders");
  },

  async createFolder(path: string): Promise<FolderChange> {
    if (!isTauri()) return mockCreateFolder(path);
    return invoke<FolderChange>("create_folder", { path });
  },

  async renameFolder(folderId: number, name: string): Promise<FolderChange> {
    if (!isTauri()) return mockRenameFolder(folderId, name);
    return invoke<FolderChange>("rename_folder", { folderId, name });
  },

  async moveFolder(folderId: number, parentId: number): Promise<FolderChange> {
    if (!isTauri()) return mockMoveFolder(folderId, parentId);
    return invoke<FolderChange>("move_folder", { folderId, parentId });
  },

  async reorderFolders(
    folderIds: number[],
    parentId: number,
    sortIndex: number,
  ): Promise<FolderChange> {
    if (!isTauri()) return mockReorderFolders(folderIds, parentId, sortIndex);
    return invoke<FolderChange>("reorder_folders", { folderIds, parentId, sortIndex });
  },

  async placeFolders(
    folderIds: number[],
    parentId: number,
    sortIndex: number | null,
  ): Promise<FolderChange> {
    if (!isTauri()) return mockPlaceFolders(folderIds, parentId, sortIndex);
    return invoke<FolderChange>("place_folders", { folderIds, parentId, sortIndex });
  },

  async applyFolderOrder(
    folderId: number,
    key: FolderSortKey,
    descending: boolean,
    recursive: boolean,
    orders: FolderOrder[],
  ): Promise<FolderChange> {
    if (!isTauri()) {
      return mockApplyFolderOrder(folderId, key, descending, recursive, orders);
    }
    return invoke<FolderChange>("apply_folder_order", {
      folderId,
      key,
      descending,
      recursive,
      orders,
    });
  },

  async deleteFolder(folderId: number): Promise<FolderChange> {
    if (!isTauri()) return mockDeleteFolder(folderId);
    return invoke<FolderChange>("delete_folder", { folderId });
  },

  async getTrashFolders(): Promise<TrashFolders> {
    if (!isTauri()) return mockTrashFolders();
    return invoke<TrashFolders>("get_trash_folders");
  },

  async restoreFolder(folderId: number): Promise<FolderChange> {
    if (!isTauri()) return mockRestoreFolder(folderId);
    return invoke<FolderChange>("restore_folder", { folderId });
  },

  async deleteFolderPermanently(folderId: number): Promise<FolderChange> {
    if (!isTauri()) return mockDeleteFolderPermanently(folderId);
    return invoke<FolderChange>("delete_folder_permanently", { folderId });
  },

  /** `includeEmpty` 只在标签管理器用：把 0 张标签也列出来，方便一次清理。 */
  async getTags(limit = 500, includeEmpty = false): Promise<TagInfo[]> {
    if (!isTauri()) return mockTags().slice(0, limit);
    return invoke<TagInfo[]>("get_tags", { limit, includeEmpty });
  },

  async searchTags(query: string, limit = 50, includeEmpty = false): Promise<TagInfo[]> {
    if (!isTauri()) return mockSearchTags(query, limit);
    return invoke<TagInfo[]>("search_tags", { query, limit, includeEmpty });
  },

  async getImageDetail(imageId: number): Promise<ImageDetails> {
    if (!isTauri()) return mockImageDetail(imageId);
    return invoke<ImageDetails>("get_image_detail", { imageId });
  },

  /** 批量取存在与回收站状态；用于外部变化后复核选择、相似参考与查看器。 */
  async getImageStates(imageIds: number[]): Promise<ImageState[]> {
    if (imageIds.length === 0) return [];
    if (!isTauri()) return mockImageStates(imageIds);
    return invoke<ImageState[]>("get_image_states", { imageIds });
  },

  /** 选中集合内各标签的拥有数；只用于批量标签候选。 */
  async getSelectionTags(imageIds: number[]): Promise<SelectionTags> {
    if (imageIds.length === 0) return { total: 0, tags: [] };
    if (!isTauri()) return mockSelectionTags(imageIds);
    return invoke<SelectionTags>("get_selection_tags", { imageIds });
  },

  async updateImageScore(imageId: number, score: number): Promise<void> {
    if (!isTauri()) {
      mockUpdateScore(imageId, score);
      return;
    }
    return invoke("update_image_score", { imageId, score });
  },

  async setImagesScore(imageIds: number[], score: number): Promise<number> {
    if (!isTauri()) return mockSetScores(imageIds, score);
    return invoke<number>("set_images_score", { imageIds, score });
  },

  async updateImageDescription(imageId: number, description: string): Promise<void> {
    if (!isTauri()) {
      mockUpdateDescription(imageId, description);
      return;
    }
    return invoke("update_image_description", { imageId, description });
  },

  async setImageTags(imageId: number, tagNames: string[]): Promise<TagRef[]> {
    if (!isTauri()) return mockSetImageTags(imageId, tagNames);
    return invoke<TagRef[]>("set_image_tags", { imageId, tagNames });
  },

  async addTagsToImages(imageIds: number[], tagNames: string[]): Promise<number> {
    if (!isTauri()) return mockAddTags(imageIds, tagNames);
    return invoke<number>("add_tags_to_images", { imageIds, tagNames });
  },

  async removeTagsFromImages(imageIds: number[], tagNames: string[]): Promise<number> {
    if (!isTauri()) return mockRemoveTags(imageIds, tagNames);
    return invoke<number>("remove_tags_from_images", { imageIds, tagNames });
  },

  async renameTag(tagId: number, name: string): Promise<void> {
    if (!isTauri()) {
      mockRenameTag(tagId, name);
      return;
    }
    return invoke("rename_tag", { tagId, name });
  },

  async deleteTag(tagId: number): Promise<number> {
    if (!isTauri()) return mockDeleteTag(tagId);
    return invoke<number>("delete_tag", { tagId });
  },

  /** 一次删掉全部 0 张标签；只写一条 `tagDelete` 记录，可撤销。 */
  async deleteZeroTags(): Promise<number> {
    if (!isTauri()) return mockDeleteTag(-1);
    return invoke<number>("delete_zero_tags");
  },

  async mergeTags(sourceId: number, targetId: number): Promise<TagMergeReport> {
    if (!isTauri()) return mockMergeTags(sourceId, targetId);
    return invoke<TagMergeReport>("merge_tags", { sourceId, targetId });
  },

  async moveImagesToFolder(imageIds: number[], folderPath: string): Promise<MoveReport> {
    if (!isTauri()) return mockMoveImages(imageIds, folderPath);
    return invoke<MoveReport>("move_images_to_folder", { imageIds, folderPath });
  },

  async trashImages(imageIds: number[]): Promise<number> {
    if (!isTauri()) return mockTrashImages(imageIds);
    return invoke<number>("trash_images", { imageIds });
  },

  async restoreImages(imageIds: number[]): Promise<number> {
    if (!isTauri()) return mockRestoreImages(imageIds);
    return invoke<number>("restore_images", { imageIds });
  },

  async deleteImagesPermanently(imageIds: number[]): Promise<DeleteReport> {
    if (!isTauri()) return mockDeleteImages(imageIds);
    return invoke<DeleteReport>("delete_images_permanently", { imageIds });
  },

  async emptyTrash(): Promise<DeleteReport> {
    if (!isTauri()) return mockEmptyTrash();
    return invoke<DeleteReport>("empty_trash");
  },

  async getOperationHistory(): Promise<HistoryPage> {
    if (!isTauri()) return mockOperationHistory();
    return invoke<HistoryPage>("get_operation_history");
  },

  async undoToOperation(targetId: number): Promise<TaskStartResult> {
    if (!isTauri()) return mockUndoToOperation(targetId);
    return invoke<TaskStartResult>("undo_to_operation", { targetId });
  },

  async redoToOperation(targetId: number): Promise<TaskStartResult> {
    if (!isTauri()) return mockRedoToOperation(targetId);
    return invoke<TaskStartResult>("redo_to_operation", { targetId });
  },

  async retryUndoOperation(targetId: number): Promise<TaskStartResult> {
    if (!isTauri()) return mockRetryUndo(targetId);
    return invoke<TaskStartResult>("retry_undo_operation", { targetId });
  },

  async skipUndoOperation(targetId: number): Promise<void> {
    if (!isTauri()) return mockSkipUndo(targetId);
    return invoke<void>("skip_undo_operation", { targetId });
  },

  async releasePendingFiles(): Promise<TaskStartResult> {
    if (!isTauri()) return mockReleasePendingFiles();
    return invoke<TaskStartResult>("release_pending_files");
  },

  async clearOperationHistory(): Promise<TaskStartResult> {
    if (!isTauri()) return mockClearOperationHistory();
    return invoke<TaskStartResult>("clear_operation_history");
  },

  async getCollections(): Promise<CollectionInfo[]> {
    if (!isTauri()) return mockCollections();
    return invoke<CollectionInfo[]>("get_collections");
  },

  async createCollection(name: string): Promise<CollectionInfo> {
    if (!isTauri()) return mockCreateCollection(name);
    return invoke<CollectionInfo>("create_collection", { name });
  },

  async renameCollection(collectionId: number, name: string): Promise<void> {
    if (!isTauri()) {
      mockRenameCollection(collectionId, name);
      return;
    }
    return invoke("rename_collection", { collectionId, name });
  },

  async deleteCollection(collectionId: number): Promise<number> {
    if (!isTauri()) return mockDeleteCollection(collectionId);
    return invoke<number>("delete_collection", { collectionId });
  },

  async reorderCollections(collectionIds: number[]): Promise<void> {
    if (!isTauri()) {
      mockReorderCollections(collectionIds);
      return;
    }
    return invoke("reorder_collections", { collectionIds });
  },

  async addImagesToCollection(collectionId: number, imageIds: number[]): Promise<number> {
    if (!isTauri()) return mockAddToCollection(collectionId, imageIds);
    return invoke<number>("add_images_to_collection", { collectionId, imageIds });
  },

  async removeImagesFromCollection(collectionId: number, imageIds: number[]): Promise<number> {
    if (!isTauri()) return mockRemoveFromCollection(collectionId, imageIds);
    return invoke<number>("remove_images_from_collection", { collectionId, imageIds });
  },

  async combineCollections(
    mode: CombineMode,
    collectionIds: number[],
    name: string,
  ): Promise<CollectionInfo> {
    if (!isTauri()) return mockCombineCollections(mode, collectionIds, name);
    return invoke<CollectionInfo>("combine_collections", { mode, collectionIds, name });
  },

  async createCollectionsFromGroups(
    groups: number[][],
    baseName: string,
  ): Promise<CollectionInfo[]> {
    if (!isTauri()) return mockCreateCollectionsFromGroups(groups, baseName);
    return invoke<CollectionInfo[]>("create_collections_from_groups", {
      groups,
      baseName,
    });
  },

  /** 合并相似组前挑出每组保留哪一张；只读，不写状态。 */
  async similarityGroupKeeps(
    groups: number[][],
    sort: SortSpec[],
  ): Promise<(number | null)[]> {
    if (!isTauri()) return groups.map((group) => group[0] ?? null);
    return invoke<(number | null)[]>("similarity_group_keeps", { groups, sort });
  },

  async getImageCapabilities(imageIds: number[]): Promise<ImageCapabilities[]> {
    if (!isTauri()) {
      return imageIds.map((imageId) => ({
        imageId,
        pose: true,
        embedding: true,
        colors: true,
        phash: true,
      }));
    }
    return invoke<ImageCapabilities[]>("get_image_capabilities", { imageIds });
  },

  async getFeatureSummary(): Promise<FeatureSummary> {
    if (!isTauri()) {
      const total = mockLibraryCounts().imageCount;
      return { description: total, embedding: total, pose: total, phash: total, colors: total };
    }
    return invoke<FeatureSummary>("get_feature_summary");
  },
};
