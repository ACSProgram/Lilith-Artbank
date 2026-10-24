import type { LibraryQuery } from "../modules/library/types";

/** 批量任务对话框需要的范围信息；由浏览界面算好，避免对话框再查一遍。 */
export interface BatchTaskContextInfo {
  selectedIds: number[];
  query: LibraryQuery | null;
  viewTotal: number | null;
  folderId: number | null;
  folderName: string | null;
  folderTotal: number | null;
  collectionId: number | null;
  collectionName: string | null;
  collectionTotal: number | null;
  libraryTotal: number;
}
