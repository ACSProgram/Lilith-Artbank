import { queryIdentityKey } from "../modules/library/model.ts";
import type { TaskScope } from "../shared/scope";
import type {
  BatchTaskKind,
  BatchTaskPreview,
  BatchTaskPreviewCapability,
} from "./taskTypes";

export interface BatchPreviewKeyInput {
  libraryId: string;
  kind: BatchTaskKind;
  scope: TaskScope | null;
  overwrite: boolean;
  capability: BatchTaskPreviewCapability | null;
  dataVersion: number;
}

export interface BatchStartConditions {
  kindEnabled: boolean;
  scopeEnabled: boolean;
  embeddingDimMissing: boolean;
  previewLoading: boolean;
  willProcess: number | null;
  /** 预览失败不阻止开始；任务开始后会给出实际数字。 */
  previewFailed?: boolean;
}

export interface StartDecision {
  enabled: boolean;
  reason: string | null;
}

export interface PreviewSummaryInput {
  total: number | null;
  willProcess: number | null;
  loading: boolean;
  offline: boolean;
  dimMissing: boolean;
  failed: boolean;
}

/** 几十万 id 不拼进键里，用长度 + 32 位 FNV-1a 区分内容。 */
function hashIds(ids: number[]): string {
  let hash = 0x811c9dc5;
  for (const id of ids) {
    let value = id >>> 0;
    for (let index = 0; index < 4; index += 1) {
      hash ^= value & 0xff;
      hash = Math.imul(hash, 0x01000193) >>> 0;
      value >>>= 8;
    }
  }
  return hash.toString(16).padStart(8, "0");
}

function scopeKey(scope: TaskScope | null): string {
  if (!scope) return "none";
  switch (scope.type) {
    case "selected":
      return `selected:${scope.imageIds.length}:${hashIds(scope.imageIds)}`;
    case "folder":
      return `folder:${scope.folderId}:${scope.includeSubfolders ? 1 : 0}`;
    case "collection":
      return `collection:${scope.collectionId}`;
    case "view":
      return `view:${queryIdentityKey(scope.query)}`;
    case "temp":
      return "temp";
    case "library":
      return "library";
  }
}

function capabilityKey(capability: BatchTaskPreviewCapability | null): string {
  if (!capability || capability.dim <= 0 || !capability.model.trim()) return "none";
  return `${capability.model}@${capability.dim}`;
}

/**
 * 预览键包含图库、数据版本、类型、跳过选项、范围与模型维度。
 *
 * 数据版本变化时不复用上一个版本的 M：旧数字可能让用户判断错要不要开始。
 */
export function batchPreviewKey(input: BatchPreviewKeyInput): string {
  return [
    input.libraryId,
    input.dataVersion,
    input.kind,
    input.overwrite ? 1 : 0,
    scopeKey(input.scope),
    capabilityKey(input.capability),
  ].join("|");
}

export interface BatchPreviewCache<T> {
  get(key: string): T | undefined;
  set(key: string, value: T): void;
  clear(): void;
  readonly size: number;
}

/** 最久未用淘汰的小缓存；同一数据版本内复用，避免来回切换时反复查询。 */
export function createBatchPreviewCache<T>(capacity = 24): BatchPreviewCache<T> {
  const entries = new Map<string, T>();
  return {
    get(key) {
      if (!entries.has(key)) return undefined;
      const value = entries.get(key) as T;
      entries.delete(key);
      entries.set(key, value);
      return value;
    },
    set(key, value) {
      entries.delete(key);
      entries.set(key, value);
      while (entries.size > capacity) {
        const oldest = entries.keys().next().value;
        if (oldest === undefined) break;
        entries.delete(oldest);
      }
    },
    clear() {
      entries.clear();
    },
    get size() {
      return entries.size;
    },
  };
}

export function canStartBatchTask(conditions: BatchStartConditions): StartDecision {
  if (!conditions.kindEnabled) {
    return { enabled: false, reason: "当前任务类型不可用" };
  }
  if (!conditions.scopeEnabled) {
    return { enabled: false, reason: "当前范围不可用" };
  }
  if (conditions.embeddingDimMissing) {
    return { enabled: false, reason: "AI 服务未返回模型维度" };
  }
  if (conditions.previewLoading) {
    return { enabled: false, reason: "正在计算待处理数量" };
  }
  if (conditions.previewFailed) {
    return { enabled: true, reason: null };
  }
  if (conditions.willProcess === 0) {
    return { enabled: false, reason: "没有需要处理的图片" };
  }
  return { enabled: true, reason: null };
}

export function previewSummary(input: PreviewSummaryInput): string {
  const total = input.total != null ? input.total.toLocaleString() : "—";
  if (input.offline) return `范围 ${total} 张 · 连接 AI 服务后计算`;
  if (input.dimMissing) return `范围 ${total} 张 · AI 服务未返回模型维度`;
  if (input.failed) return `范围 ${total} 张 · 暂时无法计算待处理数量`;
  if (input.loading || input.willProcess == null) {
    return `范围 ${total} 张 · 将处理 —`;
  }
  const willProcess = input.willProcess.toLocaleString();
  const empty = input.willProcess === 0 ? " · 没有需要处理的图片" : "";
  return `范围 ${total} 张 · 将处理 ${willProcess} 张${empty}`;
}
