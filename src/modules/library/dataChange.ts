// 扩展名不能省：`node --test` 直接运行本模块的测试时按真实路径解析。
import { hasActiveFilters } from "./model.ts";
import type { ImageRow, LibraryQuery, SortSpec } from "./types";

export type ChangeKind =
  | "metadata"
  | "folder"
  | "membership"
  | "collection"
  | "tagCatalog"
  | "features"
  | "thumbnails"
  | "content"
  | "unknown"
  | "none";

export type ChangeSource = "local" | "task" | "mcp" | "import";
export type ChangeField = "score" | "tags" | "description" | "folder" | "collection";
export type FeatureKind = "description" | "embedding" | "pose";
export type TagChangeAction = "add" | "remove" | "set" | "rename" | "delete" | "merge";

export interface RowPatch {
  id: number;
  update: Partial<ImageRow>;
}

/** 一次写入引起的图库变化；由 app 层构造，LibraryView 只负责执行。 */
export interface DataChange {
  kind: ChangeKind;
  source: ChangeSource;
  /** TaskKind、MCP 工具名或本地操作名；用于诊断与保守回退。 */
  reason?: string;
  /** 已知受影响的图片；外部任务/MCP 通常拿不到。 */
  ids?: number[];
  fields?: ChangeField[];
  tagAction?: TagChangeAction;
  tagId?: number;
  /** 合并时保留的目标标签。 */
  targetTagId?: number;
  score?: number | null;
  /** 描述是否为空；外部写入只回传这个判断，不回传整段描述。 */
  descriptionEmpty?: boolean;
  targetFolderPath?: string;
  feature?: FeatureKind;
  rowPatches?: RowPatch[];
  /** 外部范围变化：调用方确认选择已经无法可靠保留。 */
  clearSelection?: boolean;
  /** 文件夹列表已由调用方更新，无需再次读取。 */
  foldersAlreadyLoaded?: boolean;
  similarity: "keep" | "clear";
}

export interface DataChangeSignal {
  revision: number;
  changes: DataChange[];
}

export type ListAction = "none" | "patch" | "refresh";
export type SelectionAction = "keep" | "prune" | "clear";

/**
 * 一次变化对各处界面状态的效果。`selection` 的 `prune` 由 `selectionAfterChange` 落实，
 * `clear` 表示连活动图与锚点一起清空。查看器是否复核活动图由 `ViewerHost` 自己按写入版本
 * 与外部变化代次决定，不在这里表达。
 */
export interface DataChangePlan {
  list: ListAction;
  selection: SelectionAction;
  folders: boolean;
  collections: boolean;
  status: boolean;
  features: boolean;
  facets: boolean;
  tagOptions: boolean;
  detail: boolean;
  similarity: "keep" | "clear";
}

export interface PlanContext {
  hasMore: boolean;
}

const EMPTY_PLAN: DataChangePlan = {
  list: "none",
  selection: "keep",
  folders: false,
  collections: false,
  status: false,
  features: false,
  facets: false,
  tagOptions: false,
  detail: false,
  similarity: "keep",
};

function isSearching(query: LibraryQuery): boolean {
  return query.search.trim().length > 0;
}

function sortsBy(query: LibraryQuery, keys: SortSpec["key"][]): boolean {
  return query.sort.some((spec) => keys.includes(spec.key));
}

/** 与 Rust `query::uses_id_cursor` 保持一致。 */
export function usesIdCursor(sort: SortSpec[]): boolean {
  return sort.length > 1 || sort[0]?.key === "folder";
}

export function needsCursorRefresh(query: LibraryQuery, hasMore: boolean): boolean {
  return hasMore && usesIdCursor(query.sort);
}

function scoreAffectsQuery(query: LibraryQuery): boolean {
  return query.filters.scores.length > 0 || sortsBy(query, ["score"]);
}

function tagsAffectQuery(query: LibraryQuery): boolean {
  return query.filters.tagIds.length > 0 || isSearching(query);
}

function descriptionAffectsQuery(query: LibraryQuery): boolean {
  return query.filters.description !== "any" || isSearching(query);
}

function folderAffectsQuery(query: LibraryQuery): boolean {
  return (
    query.filters.folderIds.length > 0 ||
    sortsBy(query, ["folder"]) ||
    isSearching(query)
  );
}

function collectionAffectsQuery(query: LibraryQuery): boolean {
  return query.view === "collection" || sortsBy(query, ["collectionPosition"]);
}

function fieldAffectsQuery(field: ChangeField, query: LibraryQuery): boolean {
  switch (field) {
    case "score":
      return scoreAffectsQuery(query);
    case "tags":
      return tagsAffectQuery(query);
    case "description":
      return descriptionAffectsQuery(query);
    case "folder":
      return folderAffectsQuery(query);
    case "collection":
      return collectionAffectsQuery(query);
  }
}

function changeAffectsQuery(change: DataChange, query: LibraryQuery): boolean {
  if (change.fields && change.fields.length > 0) {
    return change.fields.some((field) => fieldAffectsQuery(field, query));
  }
  // 外部变化没有字段信息：只要当前查询有任一筛选/搜索，就按可能受影响处理。
  return hasActiveFilters(query.filters) || isSearching(query);
}

function featureAffectsQuery(feature: FeatureKind | undefined, query: LibraryQuery): boolean {
  // 这些任务只新增特征（描述任务可能覆盖），因此只有“缺少特征”筛选和描述搜索
  // 会改变成员关系；筛选“有特征”时新写入不会让已有结果离开。
  if (feature === "description") {
    return query.filters.description === "missing" || isSearching(query);
  }
  if (feature === "embedding") return query.filters.embedding === "missing";
  if (feature === "pose") return query.filters.pose === "missing";
  return (
    query.filters.description === "missing" ||
    query.filters.embedding === "missing" ||
    query.filters.pose === "missing" ||
    isSearching(query)
  );
}

function canPatchMetadata(change: DataChange): boolean {
  if (change.rowPatches && change.rowPatches.length > 0) return true;
  return (
    (change.ids?.length ?? 0) > 0 &&
    (change.fields ?? []).some((field) => field === "score")
  );
}

function metadataPlan(
  change: DataChange,
  query: LibraryQuery,
  context: PlanContext,
): DataChangePlan {
  const relevant = changeAffectsQuery(change, query);
  const cursorRefresh = needsCursorRefresh(query, context.hasMore);
  // 外部写入拿不到受影响 id，无法就地打补丁；即使当前筛选不受影响也要重取可见行。
  const needsReload =
    change.source !== "local" && !(change.rowPatches && change.rowPatches.length > 0);
  let list: ListAction = "none";
  if (relevant || cursorRefresh || needsReload) {
    list = "refresh";
  } else if ((change.fields ?? []).includes("tags") && !canPatchMetadata(change)) {
    // 批量标签写入只返回数量，拿不到每行最终标签，只能重取可见行。
    list = "refresh";
  } else if (canPatchMetadata(change)) {
    list = "patch";
  }
  return {
    ...EMPTY_PLAN,
    list,
    selection: metadataSelection(change, query),
    features:
      (change.fields ?? []).includes("description") ||
      (change.feature === "description"),
    facets: true,
    detail: true,
    similarity: change.similarity,
  };
}

function metadataSelection(change: DataChange, query: LibraryQuery): SelectionAction {
  if (!changeAffectsQuery(change, query)) return "keep";
  if (change.source !== "local" && !(change.ids?.length)) return "clear";
  if ((change.fields ?? []).includes("tags") && change.tagAction === "add") return "keep";
  return "prune";
}

function folderPlan(change: DataChange, query: LibraryQuery): DataChangePlan {
  const leavesScope =
    (change.ids?.length ?? 0) > 0 &&
    change.targetFolderPath != null &&
    query.filters.folderIds.length > 0;
  return {
    ...EMPTY_PLAN,
    list: "refresh",
    selection: change.clearSelection
      ? "clear"
      : leavesScope
        ? "prune"
        : "keep",
    folders: !change.foldersAlreadyLoaded,
    facets: true,
    detail: true,
    similarity: change.similarity,
  };
}

function membershipPlan(change: DataChange): DataChangePlan {
  const imported = change.source === "import" || change.reason === "import";
  return {
    ...EMPTY_PLAN,
    list: "refresh",
    selection:
      change.clearSelection || change.source !== "local" ? "clear" : "prune",
    folders: true,
    collections: !imported,
    status: true,
    features: true,
    facets: true,
    tagOptions: imported,
    detail: true,
    similarity: change.similarity,
  };
}

function collectionPlan(change: DataChange, query: LibraryQuery): DataChangePlan {
  const inCollection = collectionAffectsQuery(query);
  return {
    ...EMPTY_PLAN,
    list: inCollection ? "refresh" : "none",
    selection: change.clearSelection
      ? "clear"
      : inCollection && ((change.ids?.length ?? 0) > 0 || change.source !== "local")
        ? "prune"
        : "keep",
    collections: true,
    facets: inCollection,
    detail: (change.ids?.length ?? 0) > 0,
    similarity: change.similarity,
  };
}

function tagCatalogPlan(change: DataChange, query: LibraryQuery): DataChangePlan {
  // 合并同样会删掉源标签，筛选里含它时结果集必然变空。
  const deletesFilteredTag =
    (change.tagAction === "delete" || change.tagAction === "merge") &&
    change.tagId != null &&
    query.filters.tagIds.includes(change.tagId);
  return {
    ...EMPTY_PLAN,
    list: "refresh",
    selection: deletesFilteredTag ? "clear" : "keep",
    tagOptions: true,
    facets: true,
    detail: true,
    similarity: change.similarity,
  };
}

function featuresPlan(
  change: DataChange,
  query: LibraryQuery,
  context: PlanContext,
): DataChangePlan {
  const membership = featureAffectsQuery(change.feature, query);
  return {
    ...EMPTY_PLAN,
    list: membership || needsCursorRefresh(query, context.hasMore) ? "refresh" : "none",
    selection: !membership
      ? "keep"
      : (change.ids?.length ?? 0) > 0
        ? "prune"
        : "clear",
    features: true,
    facets: true,
    detail: true,
    similarity: change.similarity,
  };
}

function contentPlan(
  change: DataChange,
  query: LibraryQuery,
  context: PlanContext,
): DataChangePlan {
  const relevant =
    sortsBy(query, ["modifiedAt", "size", "pixels"]) ||
    query.filters.sizeMin != null ||
    query.filters.sizeMax != null ||
    query.filters.embedding === "has" ||
    query.filters.pose === "has";
  return {
    ...EMPTY_PLAN,
    list:
      relevant || needsCursorRefresh(query, context.hasMore) || !change.rowPatches?.length
        ? "refresh"
        : "patch",
    selection: "keep",
    features: true,
    facets: true,
    detail: true,
    similarity: change.similarity,
  };
}

/** 数据变化 → 各类界面状态的效果。纯函数，便于测试与复用。 */
/**
 * 这次变化是否可能改了标签关系或标签目录；批量侧栏的移除候选只在它为真时重算。
 * `unknown`（撤销/重做、认不出的外部写入）按“可能改了标签”处理，宁可多算一次。
 */
export function touchesTags(change: DataChange): boolean {
  if (change.kind === "tagCatalog" || change.kind === "unknown") return true;
  return (change.fields ?? []).includes("tags");
}

export function planDataChange(
  change: DataChange,
  query: LibraryQuery,
  context: PlanContext,
): DataChangePlan {
  switch (change.kind) {
    case "none":
      return EMPTY_PLAN;
    case "metadata":
      return metadataPlan(change, query, context);
    case "folder":
      return folderPlan(change, query);
    case "membership":
      return membershipPlan(change);
    case "collection":
      return collectionPlan(change, query);
    case "tagCatalog":
      return tagCatalogPlan(change, query);
    case "features":
      return featuresPlan(change, query, context);
    case "thumbnails":
      return {
        ...EMPTY_PLAN,
        list: "refresh",
        detail: true,
        similarity: change.similarity,
      };
    case "content":
      return contentPlan(change, query, context);
    case "unknown":
      return {
        ...EMPTY_PLAN,
        list: "refresh",
        selection: "clear",
        folders: true,
        collections: true,
        status: true,
        features: true,
        facets: true,
        tagOptions: true,
        detail: true,
        similarity: "clear",
      };
  }
}

function pathInFolderScope(target: string, folderPaths: string[], direct: boolean): boolean {
  return folderPaths.some((path) =>
    direct
      ? target === path
      : path === "" || target === path || target.startsWith(`${path}/`),
  );
}

/** 从当前选择中按这次变化移除已不在结果里的图片。 */
export function selectionAfterChange(
  selected: Set<number>,
  change: DataChange,
  query: LibraryQuery,
  folderPaths: string[],
): Set<number> {
  if (selected.size === 0 || change.kind === "none") return selected;
  if (change.clearSelection || change.kind === "unknown") return new Set();

  if (change.kind === "membership") {
    if (change.source !== "local" || !change.ids) return new Set();
    return new Set([...selected].filter((id) => !change.ids!.includes(id)));
  }

  if (change.kind === "folder") {
    if (
      change.ids?.length &&
      change.targetFolderPath != null &&
      query.filters.folderIds.length > 0 &&
      !pathInFolderScope(
        change.targetFolderPath,
        folderPaths,
        query.filters.folderDirect === true,
      )
    ) {
      return new Set([...selected].filter((id) => !change.ids!.includes(id)));
    }
    return selected;
  }

  if (change.kind === "collection") {
    if (collectionAffectsQuery(query) && change.ids?.length) {
      return new Set([...selected].filter((id) => !change.ids!.includes(id)));
    }
    return selected;
  }

  if (change.kind === "tagCatalog") {
    if (
      (change.tagAction === "delete" || change.tagAction === "merge") &&
      change.tagId != null &&
      query.filters.tagIds.includes(change.tagId)
    ) {
      return new Set();
    }
    return selected;
  }

  if (change.kind === "features") {
    if (!featureAffectsQuery(change.feature, query)) return selected;
    if (!change.ids?.length) return new Set();
    return new Set([...selected].filter((id) => !change.ids!.includes(id)));
  }

  if (change.kind !== "metadata") return selected;
  if (!changeAffectsQuery(change, query)) return selected;
  if (change.source !== "local" && !change.ids?.length) return new Set();

  const fields = change.fields ?? [];
  const ids = new Set(change.ids ?? []);
  if (ids.size === 0) return selected;

  if (fields.includes("score") && query.filters.scores.length > 0) {
    if (change.score == null || !query.filters.scores.includes(change.score)) {
      return new Set([...selected].filter((id) => !ids.has(id)));
    }
  }

  if (fields.includes("tags") && tagsAffectQuery(query) && change.tagAction !== "add") {
    return new Set([...selected].filter((id) => !ids.has(id)));
  }

  if (fields.includes("description") && descriptionAffectsQuery(query)) {
    const known = change.descriptionEmpty != null;
    const empty = change.descriptionEmpty === true;
    const leavesHas = query.filters.description === "has" && known && empty;
    const leavesMissing = query.filters.description === "missing" && known && !empty;
    // 描述搜索命中的图片可能因为描述被覆盖而离开结果，拿不到旧文本时按离开处理。
    const searchMayLeave = isSearching(query);
    if (leavesHas || leavesMissing || searchMayLeave) {
      return new Set([...selected].filter((id) => !ids.has(id)));
    }
  }

  return selected;
}

/**
 * 任务种类 → 变化。kind 用字符串接收：任务类型定义在 `src/app`，模块不反向依赖；
 * 认不出的种类按“数据未变”处理。
 */
export function dataChangeForTaskKind(kind: string): DataChange {
  switch (kind) {
    case "import":
      return {
        kind: "membership",
        source: "import",
        reason: kind,
        clearSelection: true,
        similarity: "clear",
      };
    case "thumbnail":
      return { kind: "thumbnails", source: "task", reason: kind, similarity: "keep" };
    case "aiEmbedding":
      return {
        kind: "features",
        source: "task",
        reason: kind,
        feature: "embedding",
        similarity: "clear",
      };
    case "aiPose":
      return {
        kind: "features",
        source: "task",
        reason: kind,
        feature: "pose",
        similarity: "clear",
      };
    case "aiDescription":
      return {
        kind: "features",
        source: "task",
        reason: kind,
        feature: "description",
        similarity: "clear",
      };
    case "folderMove":
      return {
        kind: "folder",
        source: "task",
        reason: kind,
        fields: ["folder"],
        clearSelection: true,
        similarity: "keep",
      };
    case "permanentDelete":
    case "emptyTrash":
      return {
        kind: "membership",
        source: "task",
        reason: kind,
        clearSelection: true,
        similarity: "clear",
      };
    case "undo":
    case "redo":
      return {
        kind: "unknown",
        source: "task",
        reason: kind,
        clearSelection: true,
        similarity: "clear",
      };
    case "releaseGarbage":
      return { kind: "none", source: "task", reason: kind, similarity: "keep" };
    case "optimize":
      return {
        kind: "content",
        source: "task",
        reason: kind,
        similarity: "clear",
      };
    case "tempFeatures":
      return {
        kind: "features",
        source: "task",
        reason: kind,
        similarity: "clear",
      };
    default:
      return {
        kind: "none",
        source: "task",
        reason: kind,
        similarity: "keep",
      };
  }
}

export interface McpChangePayload {
  tool?: string;
  invalidateSimilarity?: boolean;
  change?: {
    kind?: string;
    fields?: ChangeField[];
    tagAction?: TagChangeAction;
    tagId?: number;
    targetTagId?: number;
    score?: number | null;
    descriptionEmpty?: boolean;
  } | null;
}

/** 结构化 `change.kind` 优先；旧版后端没有这个字段时按工具名回退。 */
function kindFromStructured(kind: string | undefined, tool: string): ChangeKind {
  if (
    kind === "metadata" ||
    kind === "folder" ||
    kind === "membership" ||
    kind === "collection" ||
    kind === "tagCatalog"
  ) {
    return kind;
  }
  if (tool === "images_update") return "metadata";
  if (tool === "images_trash" || tool === "images_restore") return "membership";
  if (tool.startsWith("collections_")) return "collection";
  if (tool === "folders_modify") return "folder";
  return "unknown";
}

export function dataChangeForMcp(payload: McpChangePayload): DataChange {
  const tool = payload.tool ?? "";
  const structured = payload.change ?? null;
  const kind = kindFromStructured(structured?.kind, tool);
  const clearSelection =
    tool === "images_trash" ||
    tool === "images_restore" ||
    tool === "collections_membership";
  return {
    kind,
    source: "mcp",
    reason: tool,
    fields: structured?.fields,
    tagAction: structured?.tagAction,
    tagId: structured?.tagId,
    targetTagId: structured?.targetTagId,
    score: structured?.score,
    descriptionEmpty: structured?.descriptionEmpty,
    clearSelection,
    similarity: payload.invalidateSimilarity ? "clear" : "keep",
  };
}

/** 同一批变化里每个维度取最保守的动作：refresh > patch > none、clear > prune > keep。 */
export function combinePlans(plans: DataChangePlan[]): DataChangePlan {
  return plans.reduce<DataChangePlan>(
    (merged, plan) => ({
      list: stronger(merged.list, plan.list, LIST_ORDER),
      selection: stronger(merged.selection, plan.selection, SELECTION_ORDER),
      folders: merged.folders || plan.folders,
      collections: merged.collections || plan.collections,
      status: merged.status || plan.status,
      features: merged.features || plan.features,
      facets: merged.facets || plan.facets,
      tagOptions: merged.tagOptions || plan.tagOptions,
      detail: merged.detail || plan.detail,
      similarity: stronger(merged.similarity, plan.similarity, SIMILARITY_ORDER),
    }),
    { ...EMPTY_PLAN },
  );
}

const LIST_ORDER: ListAction[] = ["none", "patch", "refresh"];
const SELECTION_ORDER: SelectionAction[] = ["keep", "prune", "clear"];
const SIMILARITY_ORDER: DataChangePlan["similarity"][] = ["keep", "clear"];

/** 按给定顺序取更靠后的值；顺序表从最宽松排到最保守。 */
function stronger<T extends string>(left: T, right: T, order: T[]): T {
  return order.indexOf(right) > order.indexOf(left) ? right : left;
}
