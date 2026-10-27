import type { FolderChange, FolderInfo, FolderOrder, FolderSortKey } from "./types";

export type { FolderOrder, FolderSortKey } from "./types";

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

export const FOLDER_SORT_LABELS: Record<FolderSortKey, string> = {
  name: "名称",
  total: "图片数（含子文件夹）",
};

export function folderName(path: string): string {
  const index = path.lastIndexOf("/");
  return index < 0 ? path : path.slice(index + 1);
}

export function parentFolderPath(path: string): string {
  const index = path.lastIndexOf("/");
  return index < 0 ? "" : path.slice(0, index);
}

/**
 * 把写命令的增量结果合进本地文件夹数组：按 id 替换或追加，删除只剔除 id。
 * 目录顺序由树模型按 sortIndex/id 重排，这里不关心顺序。
 */
export function applyFolderChange(
  current: FolderInfo[],
  change: FolderChange,
): FolderInfo[] {
  const removed = new Set(change.removedIds);
  const updates = new Map(change.folders.map((folder) => [folder.id, folder]));
  const next: FolderInfo[] = [];
  const seen = new Set<number>();
  for (const folder of current) {
    if (removed.has(folder.id)) continue;
    const update = updates.get(folder.id);
    next.push(update ?? folder);
    seen.add(folder.id);
  }
  for (const folder of change.folders) {
    if (!seen.has(folder.id)) next.push(folder);
  }
  return next;
}

export function buildFolderTree(folders: FolderInfo[]): FolderTreeModel {
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

  const compareNodes = (left: FolderNode, right: FolderNode) =>
    left.sortIndex - right.sortIndex || left.id - right.id;
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

/** 某个文件夹的直接子文件夹；根目录返回顶层节点，id 不存在时返回空数组。 */
export function childrenOf(tree: FolderTreeModel, folderId: number): FolderNode[] {
  if (tree.rootId === folderId) return tree.nodes;
  return tree.byId.get(folderId)?.children ?? [];
}

function compareByKey(key: FolderSortKey, descending: boolean) {
  const direction = descending ? -1 : 1;
  return (left: FolderNode, right: FolderNode) => {
    const difference =
      key === "name"
        ? collator.compare(left.name, right.name)
        : left.totalImageCount - right.totalImageCount;
    return difference !== 0 ? difference * direction : left.id - right.id;
  };
}

/** 一次性排序需要写回的同级顺序；没有可调整的父级时返回空数组。 */
export function sortFolderOrders(
  tree: FolderTreeModel,
  rootId: number,
  key: FolderSortKey,
  descending: boolean,
  recursive: boolean,
): FolderOrder[] {
  const compareNodes = compareByKey(key, descending);
  const order = (items: FolderNode[], parentId: number): FolderOrder => ({
    parentId,
    folderIds: [...items].sort(compareNodes).map((node) => node.id),
  });

  const rootChildren = childrenOf(tree, rootId);
  const orders: FolderOrder[] = [];
  const visit = (items: FolderNode[], parentId: number) => {
    if (items.length >= 2) orders.push(order(items, parentId));
    for (const item of items) visit(item.children, item.id);
  };

  if (recursive) visit(rootChildren, rootId);
  else if (rootChildren.length >= 2) orders.push(order(rootChildren, rootId));
  return orders;
}

/** 该文件夹自身或任一后代是否有可排序的同级；没有时排序菜单项置灰。 */
export function needsFolderSort(tree: FolderTreeModel, folderId: number): boolean {
  const hasSortable = (items: FolderNode[]): boolean =>
    items.length >= 2 || items.some((item) => hasSortable(item.children));
  return hasSortable(childrenOf(tree, folderId));
}

/** 该文件夹的后代里是否有可排序的同级；只用于决定是否允许勾选递归。 */
export function hasNestedSortable(tree: FolderTreeModel, folderId: number): boolean {
  const hasSortableBelow = (items: FolderNode[]): boolean =>
    items.some((item) => item.children.length >= 2 || hasSortableBelow(item.children));
  return hasSortableBelow(childrenOf(tree, folderId));
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

export function expandedPaths(ids: Set<number>, byId: Map<number, FolderNode>): string[] {
  return [...ids]
    .map((id) => byId.get(id)?.path)
    .filter((path): path is string => path != null);
}

export function resolveExpandedPaths(
  paths: string[],
  byPath: Map<string, FolderNode>,
): Set<number> {
  const ids = new Set<number>();
  for (const path of paths) {
    const node = byPath.get(path);
    if (node) ids.add(node.id);
  }
  return ids;
}

export function defaultExpandedIds(nodes: FolderNode[]): Set<number> {
  return new Set(nodes.map((node) => node.id));
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

/** 按 id 取出文件夹，保持传入顺序，不存在的 id 直接跳过。 */
export function foldersById(folders: FolderInfo[], ids: number[]): FolderInfo[] {
  return ids
    .map((id) => folders.find((folder) => folder.id === id))
    .filter((folder): folder is FolderInfo => folder != null);
}

/** 去掉被另一个选中文件夹包含的后代，只保留最外层；顺序按路径深度、id。 */
export function outermostFolders<T extends { id: number; path: string }>(folders: T[]): T[] {
  const sorted = [...folders].sort(
    (left, right) => left.path.length - right.path.length || left.id - right.id,
  );
  return sorted.filter(
    (folder) =>
      !sorted.some(
        (other) =>
          other.id !== folder.id && isSameOrDescendant(folder.path, other.path),
      ),
  );
}
