/**
 * 维护页与后端契约同形的类型。
 *
 * 字段名对应 Rust 侧的 `serde(rename_all = "camelCase")`：
 * `library/inspect.rs`（检查报告）、`library/pack_gc.rs`（段统计与回收落点）。
 */

export type InspectionMode = "fast" | "full";

/** 报告里的四类问题；与 Rust `report_csv_group` 的分组名一致。 */
export type ReportGroup = "missing" | "orphan" | "sizeMismatch" | "hashMismatch";

/** 后端每份清单最多保留多少条；超过之后 `truncated` 为真。 */
export const REPORT_ITEM_LIMIT = 5000;

export interface MissingRow {
  imageId: number;
  /** 图库内相对路径。 */
  path: string;
  expectedSize: number;
}

export interface OrphanFile {
  path: string;
  size: number;
  /** 文件名里的哈希属于别的文件夹；为空表示库里根本没有这个哈希。 */
  otherFolder: string | null;
}

export interface SizeMismatch {
  imageId: number;
  path: string;
  expectedSize: number;
  actualSize: number;
}

export interface HashMismatch {
  imageId: number;
  path: string;
}

export interface InspectionReport {
  taskId: string;
  libraryId: string;
  mode: InspectionMode;
  scannedFolders: number;
  scannedRows: number;
  scannedFiles: number;
  missingTotal: number;
  orphanTotal: number;
  sizeMismatchTotal: number;
  hashMismatchTotal: number;
  nonContentFiles: number;
  /** 扫描期间被改动、因而跳过哈希校验的行数。 */
  changedDuringScan: number;
  hashChecked: number;
  missing: MissingRow[];
  orphans: OrphanFile[];
  sizeMismatch: SizeMismatch[];
  hashMismatch: HashMismatch[];
  /** 清单超过上限被截断：界面提示只列出前 N 条。 */
  truncated: boolean;
  elapsedMs: number;
  finishedAt: number;
}

export interface SegmentStat {
  id: number;
  fileName: string;
  size: number;
  used: number;
  blobs: number;
  /** 段里没有被任何行引用的字节（段头不计）。 */
  garbage: number;
}

export interface FeaturePackStat {
  kind: string;
  model: string;
  dim: number;
  packName: string;
  blobs: number;
  used: number;
  fileSize: number;
  garbage: number;
}

export interface CompactMark {
  at: number;
  freedBytes: number;
}

export interface PackStats {
  segments: SegmentStat[];
  features: FeaturePackStat[];
  totalSize: number;
  totalUsed: number;
  garbageBytes: number;
  lastCompact: CompactMark | null;
  /** 段垃圾率超过它才算值得回收。 */
  triggerRatio: number;
}

export function segmentGarbageRatio(segment: SegmentStat): number {
  return segment.size > 0 ? segment.garbage / segment.size : 0;
}

/** 报告是否一条问题都没有；界面据此显示「没有发现问题」而不是四个 0。 */
export function reportIsClean(report: InspectionReport): boolean {
  return (
    report.missingTotal === 0 &&
    report.orphanTotal === 0 &&
    report.sizeMismatchTotal === 0 &&
    report.hashMismatchTotal === 0
  );
}

/** 相对时间只用于「上次回收」这一行；超过一天就写日期，不再数天数。 */
export function formatRelative(at: number, now = Date.now()): string {
  const delta = Math.max(0, now - at);
  const minute = 60_000;
  if (delta < minute) return "刚刚";
  if (delta < 60 * minute) return `${Math.floor(delta / minute)} 分钟前`;
  if (delta < 24 * 60 * minute) return `${Math.floor(delta / (60 * minute))} 小时前`;
  return new Date(at).toLocaleDateString();
}
