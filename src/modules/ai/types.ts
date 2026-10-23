export type AiJobKind = "embedding" | "pose" | "description";
export type AiJobPhase =
  | "preparing"
  | "running"
  | "completed"
  | "cancelled"
  | "failed";
export type AiProcessState =
  | "stopped"
  | "starting"
  | "running"
  | "external"
  | "failed";
export type AiMeanPhase = "missing" | "computing" | "ready" | "failed";

export interface AiCapability {
  ready: boolean;
  model: string;
  dim: number | null;
}

export interface AiHealth {
  status: string;
  version: string | null;
  capabilities: Record<string, AiCapability>;
}

export interface AiProcessStatus {
  state: AiProcessState;
  owned: boolean;
  message: string | null;
  attempts: number;
}

export interface AiMeanStatus {
  phase: AiMeanPhase;
  model: string | null;
  dim: number | null;
  count: number;
  message: string | null;
}

export interface AiJobFailure {
  imageId: number | null;
  reason: string;
}

export interface AiJobProgress {
  jobId: string;
  kind: AiJobKind;
  phase: AiJobPhase;
  total: number;
  completed: number;
  failed: number;
  skipped: number;
  currentImageId: number | null;
  message: string;
  elapsedMs: number;
}

export interface AiJobSnapshot extends AiJobProgress {
  startedAt: number;
  finishedAt: number | null;
  failures: AiJobFailure[];
}

export interface AiStatus {
  baseUrl: string;
  autoStart: boolean;
  executableConfigured: boolean;
  online: boolean;
  process: AiProcessStatus;
  health: AiHealth | null;
  mean: AiMeanStatus;
  job: AiJobSnapshot | null;
}

export interface AiJobRequest {
  kind: AiJobKind;
  imageIds: number[];
  /** 图库查询条件（不透明透传，由后端反序列化）；AI 模块不依赖图库类型。 */
  query: object | null;
  overwrite: boolean;
  prompt?: string | null;
  maxTokens?: number | null;
}

export interface SemanticSearchResult {
  token: string;
  model: string;
  dim: number;
  total: number;
  elapsedMs: number;
  meanCount: number;
}

export interface SimilarityPage<T> {
  items: T[];
  nextOffset: number | null;
  total: number;
}

export interface AiDescriptionResult {
  imageId: number;
  model: string;
  text: string;
}

export interface PoseLandmark {
  x: number;
  y: number;
  z: number;
  visibility: number;
}

export interface PoseData {
  model: string;
  landmarks: PoseLandmark[];
}

export const DEFAULT_DESCRIPTION_PROMPT =
  "用中文简短描述画面主体、风格、色调与构图。";
