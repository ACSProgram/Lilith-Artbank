import { invoke } from "@tauri-apps/api/core";
import {
  mockFilterCounts,
  mockFolders,
  mockQueryIds,
  mockQueryPage,
  mockTags,
} from "./mock";
import type {
  FilterCounts,
  FolderInfo,
  ImagePage,
  LibraryQuery,
  TagInfo,
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

  async getFolders(): Promise<FolderInfo[]> {
    if (!isTauri()) return mockFolders;
    return invoke<FolderInfo[]>("get_folders");
  },

  async getTags(limit = 500): Promise<TagInfo[]> {
    if (!isTauri()) return mockTags.slice(0, limit);
    return invoke<TagInfo[]>("get_tags", { limit });
  },
};
