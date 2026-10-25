import type { FolderInfo } from "./types";

export interface FolderNode extends FolderInfo {
  name: string;
  depth: number;
  children: FolderNode[];
}

export interface FolderTreeModel {
  nodes: FolderNode[];
  byId: Map<number, FolderNode>;
  byPath: Map<string, FolderNode>;
  rootId: number | null;
}

const collator = new Intl.Collator("zh-Hans-CN", { numeric: true, sensitivity: "base" });

export type FolderSortMode = "manual" | "name" | "total" | "direct";

export interface FolderSortSpec {
  mode: FolderSortMode;
  descending: boolean;
}

export const DEFAULT_FOLDER_SORT: FolderSortSpec = { mode: "manual", descending: false };

export const FOLDER_SORT_LABELS: Record<FolderSortMode, string> = {
  manual: "手动",
  name: "名称",
  total: "图片数（含子文件夹）",
  direct: "图片数（直接）",
};

export function folderName(path: string): string {
  const index = path.lastIndexOf("/");
  return index < 0 ? path : path.slice(index + 1);
}

export function parentFolderPath(path: string): string {
  const index = path.lastIndexOf("/");
  return index < 0 ? "" : path.slice(0, index);
}

export function buildFolderTree(
  folders: FolderInfo[],
  sort: FolderSortSpec = DEFAULT_FOLDER_SORT,
): FolderTreeModel {
  const byId = new Map<number, FolderNode>();
  const byPath = new Map<string, FolderNode>();
  const rootId = folders.find((folder) => folder.path === "")?.id ?? null;

  for (const folder of folders) {
    if (!folder.path) continue;
    const node: FolderNode = {
      ...folder,
      name: folderName(folder.path),
      depth: folder.path.split("/").length - 1,
      children: [],
    };
    byId.set(folder.id, node);
    byPath.set(folder.path, node);
  }

  const nodes: FolderNode[] = [];
  for (const folder of folders) {
    if (!folder.path) continue;
    const node = byId.get(folder.id);
    if (!node) continue;
    const parent = byPath.get(parentFolderPath(folder.path));
    if (parent) parent.children.push(node);
    else nodes.push(node);
  }

  const direction = sort.descending ? -1 : 1;
  const compareNodes = (left: FolderNode, right: FolderNode) => {
    let difference = 0;
    if (sort.mode === "name") difference = collator.compare(left.name, right.name);
    else if (sort.mode === "total") difference = left.totalImageCount - right.totalImageCount;
    else if (sort.mode === "direct") difference = left.directImageCount - right.directImageCount;
    else difference = left.sortIndex - right.sortIndex;
    return difference !== 0 ? difference * direction : left.id - right.id;
  };
  const sortNodes = (items: FolderNode[]) => {
    items.sort(compareNodes);
    for (const item of items) sortNodes(item.children);
  };
  sortNodes(nodes);

  return { nodes, byId, byPath, rootId };
}

export function flattenVisible(nodes: FolderNode[], expanded: Set<number>): FolderNode[] {
  const rows: FolderNode[] = [];
  const visit = (items: FolderNode[]) => {
    for (const item of items) {
      rows.push(item);
      if (expanded.has(item.id) && item.children.length > 0) visit(item.children);
    }
  };
  visit(nodes);
  return rows;
}

/** 祖先路径（不含根与自身），例如 人物/速写/手部 -> [人物, 人物/速写]。 */
export function ancestorPaths(path: string): string[] {
  const parts = path.split("/").filter(Boolean);
  const result: string[] = [];
  let current = "";
  for (const part of parts.slice(0, -1)) {
    current = current ? `${current}/${part}` : part;
    result.push(current);
  }
  return result;
}

export function breadcrumbsFor(
  tree: FolderTreeModel,
  selectedIds: number[],
): { id: number | null; label: string; path: string }[] {
  if (selectedIds.length !== 1) return [];
  const node = tree.byId.get(selectedIds[0]);
  if (!node) return [];
  const trail: { id: number | null; label: string; path: string }[] = [
    { id: tree.rootId, label: "全部图片", path: "" },
  ];
  for (const path of [...ancestorPaths(node.path), node.path]) {
    trail.push({ id: tree.byPath.get(path)?.id ?? null, label: folderName(path), path });
  }
  return trail;
}

export function isSameOrDescendant(path: string, ancestor: string): boolean {
  return ancestor === "" || path === ancestor || path.startsWith(`${ancestor}/`);
}
