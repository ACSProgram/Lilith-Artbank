import { invoke } from "@tauri-apps/api/core";
import type { TaskStartResult } from "../../app/taskTypes";
import {
  mockAddTags,
  mockAddToCollection,
  mockClearOperationHistory,
  mockCollections,
  mockCombineCollections,
  mockCreateCollection,
  mockCreateCollectionsFromGroups,
  mockCreateFolder,
  mockDeleteCollection,
  mockDeleteFolder,
  mockDeleteImages,
  mockDeleteTag,
  mockEmptyTrash,
  mockFilterCounts,
  mockFolders,
  mockImageDetail,
  mockLibraryCounts,
  mockMoveFolder,
  mockMoveImages,
  mockOperationHistory,
  mockQueryIds,
  mockQueryPage,
  mockRedoToOperation,
  mockRemoveFromCollection,
  mockRemoveTags,
  mockRenameCollection,
  mockRenameFolder,
  mockRenameTag,
  mockReorderCollections,
  mockReorderFolders,
  mockRestoreImages,
  mockSearchTags,
  mockSetImageTags,
  mockSetScores,
  mockTags,
  mockTrashImages,
  mockUndoToOperation,
  mockUpdateDescription,
  mockUpdateScore,
} from "./mock";
import type {
  CollectionInfo,
  CombineMode,
  DeleteReport,
  FilterCounts,
  FeatureSummary,
  FolderInfo,
  HistoryPage,
  ImageCapabilities,
  ImageDetails,
  ImagePage,
  LibraryQuery,
  MoveReport,
  TagInfo,
  TagRef,
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

  async getFilterCounts(query: LibraryQuery): Promise<FilterCounts> {
    if (!isTauri()) return mockFilterCounts(query);
    return invoke<FilterCounts>("get_filter_counts", { query });
  },

  /** 只算命中总数，不取分面；对话框显示“当前视图 N 张”时用它。 */
  async getQueryTotal(query: LibraryQuery): Promise<number> {
    if (!isTauri()) return mockFilterCounts(query).total;
    return invoke<number>("get_query_total", { query });
  },

  async getFolders(): Promise<FolderInfo[]> {
    if (!isTauri()) return mockFolders();
    return invoke<FolderInfo[]>("get_folders");
  },

  async createFolder(path: string): Promise<FolderInfo[]> {
    if (!isTauri()) return mockCreateFolder(path);
    return invoke<FolderInfo[]>("create_folder", { path });
  },

  async renameFolder(folderId: number, name: string): Promise<FolderInfo[]> {
    if (!isTauri()) return mockRenameFolder(folderId, name);
    return invoke<FolderInfo[]>("rename_folder", { folderId, name });
  },

  async moveFolder(folderId: number, parentId: number): Promise<FolderInfo[]> {
    if (!isTauri()) return mockMoveFolder(folderId, parentId);
    return invoke<FolderInfo[]>("move_folder", { folderId, parentId });
  },

  async reorderFolders(
    folderIds: number[],
    parentId: number,
    sortIndex: number,
  ): Promise<FolderInfo[]> {
    if (!isTauri()) return mockReorderFolders(folderIds, parentId, sortIndex);
    return invoke<FolderInfo[]>("reorder_folders", { folderIds, parentId, sortIndex });
  },

  async deleteFolder(folderId: number): Promise<FolderInfo[]> {
    if (!isTauri()) return mockDeleteFolder(folderId);
    return invoke<FolderInfo[]>("delete_folder", { folderId });
  },

  async getTags(limit = 500): Promise<TagInfo[]> {
    if (!isTauri()) return mockTags().slice(0, limit);
    return invoke<TagInfo[]>("get_tags", { limit });
  },

  async searchTags(query: string, limit = 50): Promise<TagInfo[]> {
    if (!isTauri()) return mockSearchTags(query, limit);
    return invoke<TagInfo[]>("search_tags", { query, limit });
  },

  async getImageDetail(imageId: number): Promise<ImageDetails> {
    if (!isTauri()) return mockImageDetail(imageId);
    return invoke<ImageDetails>("get_image_detail", { imageId });
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
