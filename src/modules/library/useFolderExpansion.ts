import { useEffect, useRef, useState } from "react";
import {
  ancestorPaths,
  defaultExpandedIds,
  expandedPaths,
  resolveExpandedPaths,
  type FolderTreeModel,
} from "./folderTreeModel";

const EXPANDED_STORAGE_PREFIX = "lilith.folderExpanded.";

function readExpandedPaths(libraryId: string): string[] | null {
  try {
    const raw = localStorage.getItem(`${EXPANDED_STORAGE_PREFIX}${libraryId}`);
    if (raw == null) return null;
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed) || !parsed.every((item) => typeof item === "string")) {
      return null;
    }
    return parsed;
  } catch {
    return null;
  }
}

function writeExpandedPaths(libraryId: string, paths: string[]) {
  try {
    localStorage.setItem(`${EXPANDED_STORAGE_PREFIX}${libraryId}`, JSON.stringify(paths));
  } catch {
    // 忽略浏览器限制；展开状态仍在本会话内生效。
  }
}

interface UseFolderExpansionOptions {
  libraryId: string;
  tree: FolderTreeModel;
  trashMode: boolean;
  selectedFolderIds: number[];
}

export interface FolderExpansion {
  expanded: Set<number>;
  toggleExpanded: (id: number) => void;
  /** 拖动悬停到某个文件夹上时展开它；已在集合里时原样返回。 */
  expandFolder: (id: number) => void;
}

/**
 * 文件夹树的展开集合：回收站与正常树各自保留，正常树的展开按库写入 localStorage。
 */
export function useFolderExpansion({
  libraryId,
  tree,
  trashMode,
  selectedFolderIds,
}: UseFolderExpansionOptions): FolderExpansion {
  const [expanded, setExpanded] = useState<Set<number>>(() => new Set());
  const hydrated = useRef(false);
  const skipPersist = useRef(true);

  // 进入回收站时先展开全部顶层节点；正常树的展开状态暂存，切回后原样恢复。
  const lastTrashMode = useRef(trashMode);
  const normalExpandedRef = useRef<Set<number> | null>(null);
  useEffect(() => {
    if (lastTrashMode.current !== trashMode) {
      lastTrashMode.current = trashMode;
      if (trashMode) {
        normalExpandedRef.current = expanded;
        setExpanded(new Set(defaultExpandedIds(tree.nodes)));
      } else {
        setExpanded(normalExpandedRef.current ?? new Set(defaultExpandedIds(tree.nodes)));
        normalExpandedRef.current = null;
      }
      return;
    }
    if (trashMode && expanded.size === 0 && tree.nodes.length > 0) {
      setExpanded(new Set(defaultExpandedIds(tree.nodes)));
    }
  }, [trashMode, tree]);

  // 换库时 Sidebar 用 libraryId 作 key 重新挂载本组件，展开状态无需手动重置。
  useEffect(() => {
    if (trashMode) return;
    if (hydrated.current || tree.nodes.length === 0) return;
    hydrated.current = true;
    skipPersist.current = true;
    const saved = readExpandedPaths(libraryId);
    setExpanded(
      saved ? resolveExpandedPaths(saved, tree.byPath) : defaultExpandedIds(tree.nodes),
    );
  }, [tree, libraryId]);

  useEffect(() => {
    if (trashMode || !hydrated.current) return;
    if (skipPersist.current) {
      skipPersist.current = false;
      return;
    }
    writeExpandedPaths(libraryId, expandedPaths(expanded, tree.byId));
  }, [expanded, tree, libraryId]);

  useEffect(() => {
    if (selectedFolderIds.length === 0) return;
    setExpanded((current) => {
      const next = new Set(current);
      let changed = false;
      for (const id of selectedFolderIds) {
        const node = tree.byId.get(id);
        if (!node) continue;
        for (const path of ancestorPaths(node.path)) {
          const ancestor = tree.byPath.get(path);
          if (ancestor && !next.has(ancestor.id)) {
            next.add(ancestor.id);
            changed = true;
          }
        }
      }
      return changed ? next : current;
    });
  }, [selectedFolderIds, tree]);

  const toggleExpanded = (id: number) => {
    setExpanded((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const expandFolder = (id: number) => {
    setExpanded((current) => {
      if (current.has(id)) return current;
      const next = new Set(current);
      next.add(id);
      return next;
    });
  };

  return { expanded, toggleExpanded, expandFolder };
}
