import type { FeatureSummary, ImageCapabilities } from "../modules/library/types";
import type { SimilarKind } from "./similarity";

export interface ActionAvailability {
  enabled: boolean;
  reason?: string;
}

const ALLOW: ActionAvailability = { enabled: true };

/** 右键“找相似”各项：按目标图实际具备的特征判断，不靠点击后报错。 */
export function similarKindAvailability(
  kind: SimilarKind,
  caps: ImageCapabilities | null,
): ActionAvailability {
  if (!caps) return ALLOW;
  switch (kind) {
    case "phash":
      return caps.phash
        ? ALLOW
        : { enabled: false, reason: "这张图片缺少 pHash 特征" };
    case "color4":
    case "color8":
      return caps.colors
        ? ALLOW
        : { enabled: false, reason: "这张图片缺少主色数据" };
    case "embedding":
      return caps.embedding
        ? ALLOW
        : { enabled: false, reason: "这张图片还没有 embedding，可在批量任务里生成" };
    case "pose":
      return caps.pose
        ? ALLOW
        : { enabled: false, reason: "这张图片还没有姿势数据，可在批量任务里生成" };
    default:
      return ALLOW;
  }
}

/** AI 批量任务需要本机服务在线；具体模型由后端在开始前确保加载。 */
export function aiTaskAvailability(online: boolean): ActionAvailability {
  return online
    ? ALLOW
    : { enabled: false, reason: "AI 服务离线，批量任务需要本机 AI 服务" };
}

/** 语义搜索需要文本模型和至少一张图片 embedding。 */
export function semanticSearchAvailability(
  online: boolean,
  embeddingCount: number,
): ActionAvailability {
  if (!online) {
    return { enabled: false, reason: "AI 服务离线，语义搜索需要文本模型" };
  }
  if (embeddingCount === 0) {
    return { enabled: false, reason: "还没有图片 embedding，可在批量任务里生成" };
  }
  return ALLOW;
}

/** 筛选面板“内容状态”在某项全库为 0 时置灰。 */
export function presenceAvailability(
  field: "description" | "embedding" | "pose",
  summary: FeatureSummary | null,
): ActionAvailability {
  if (!summary) return ALLOW;
  if (summary[field] > 0) return ALLOW;
  const label =
    field === "description" ? "描述" : field === "embedding" ? "embedding" : "姿势";
  return { enabled: false, reason: `当前图库还没有${label}` };
}
