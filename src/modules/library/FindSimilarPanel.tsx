import { useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  Palette,
  PersonStanding,
  ScanSearch,
  Sparkles,
  X,
} from "lucide-react";
import { Button, IconButton } from "../../shared/components/Button";
import { ScopePicker } from "../../shared/components/ScopePicker";
import type { BatchTaskContextInfo } from "../../shared/batchTaskContext";
import {
  buildScope,
  defaultScopeChoice,
  scopeAvailability,
  scopeName,
  type ScopeChoice,
} from "../../shared/scope";
import {
  FEATURE_MODE_LABELS,
  MAX_SIMILAR_REFERENCE,
  similarKindLabel,
  type FeatureMode,
  type FindSimilarConfig,
  type SimilarKind,
} from "../../shared/similarity";
import { errorMessage } from "../../shared/errors";
import { libraryApi } from "./api";
import { LibraryThumbnail } from "./LibraryThumbnail";
import type { ImageCapabilities, ImageRow } from "./types";

export interface SimilarReference {
  ids: number[];
  /** 网格里已加载的行，用于面板缩略图；可能少于 ids。 */
  items: ImageRow[];
}

interface FindSimilarPanelProps {
  open: boolean;
  reference: SimilarReference | null;
  context: BatchTaskContextInfo | null;
  initial: FindSimilarConfig | null;
  onClose: () => void;
  onStart: (config: FindSimilarConfig) => void;
}

interface ThresholdSetting {
  limited: boolean;
  value: number;
}

type ThresholdMap = Record<SimilarKind, ThresholdSetting>;

const SINGLE_KINDS: SimilarKind[] = ["phash", "color4", "color8", "embedding", "pose"];
const AVERAGE_KINDS: SimilarKind[] = ["color4", "color8", "embedding"];
const FEATURE_MODES: FeatureMode[] = ["overall", "subject", "composition"];

/** pHash 是 256 位；界面滑杆只覆盖实际有用的前 64 位距离，“不限”才放开到全范围。 */
const PHASH_SLIDER_MAX = 64;
const PHASH_UNLIMITED = 256;

const DEFAULT_THRESHOLDS: ThresholdMap = {
  phash: { limited: true, value: 6 },
  color4: { limited: false, value: 0.15 },
  color8: { limited: false, value: 0.15 },
  embedding: { limited: false, value: 0.5 },
  pose: { limited: false, value: 0.7 },
};

const METRIC_ICONS: Record<SimilarKind, typeof ScanSearch> = {
  phash: ScanSearch,
  color4: Palette,
  color8: Palette,
  embedding: Sparkles,
  pose: PersonStanding,
};

const MIN_RESULTS = 1;
const MAX_RESULTS = 10_000;
const DEFAULT_RESULTS = 2_000;

const defaultThresholds = (): ThresholdMap => ({
  phash: { ...DEFAULT_THRESHOLDS.phash },
  color4: { ...DEFAULT_THRESHOLDS.color4 },
  color8: { ...DEFAULT_THRESHOLDS.color8 },
  embedding: { ...DEFAULT_THRESHOLDS.embedding },
  pose: { ...DEFAULT_THRESHOLDS.pose },
});

function fileNameOf(path: string): string {
  const index = Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/"));
  return index >= 0 ? path.slice(index + 1) : path;
}

function clampPercent(value: number): number {
  return Math.min(100, Math.max(0, value));
}

/** 所有指标统一成“最低相似度 0-100%”：越大越严格，避免距离上下限的两种方向混在一起。 */
function thresholdPercent(kind: SimilarKind, value: number): number {
  switch (kind) {
    case "phash":
      return clampPercent(
        Math.round((1 - Math.min(value, PHASH_SLIDER_MAX) / PHASH_SLIDER_MAX) * 100),
      );
    case "color4":
    case "color8":
      return clampPercent(Math.round((1 - value) * 100));
    case "embedding":
    case "pose":
      return clampPercent(Math.round(((value + 1) / 2) * 100));
  }
}

function percentThreshold(kind: SimilarKind, percent: number): number {
  const ratio = percent / 100;
  switch (kind) {
    case "phash":
      return Math.round((1 - ratio) * PHASH_SLIDER_MAX);
    case "color4":
    case "color8":
      return Math.round((1 - ratio) * 100) / 100;
    case "embedding":
    case "pose":
      return Math.round((ratio * 2 - 1) * 100) / 100;
  }
}

function scopeLabelOf(
  choice: ScopeChoice,
  context: BatchTaskContextInfo,
  includeSubfolders: boolean,
): string {
  const name = scopeName(choice, context);
  if (choice !== "folder") return name;
  return `${name}${includeSubfolders ? "（含子文件夹）" : "（不含子文件夹）"}`;
}

interface RadioOptionProps {
  group: string;
  label: string;
  active: boolean;
  disabled: boolean;
  /** 只在不可用时给：写在选项后面说明原因。 */
  reason?: string;
  icon?: typeof ScanSearch;
  onSelect: () => void;
}

/** 指标与语义特征模式共用的一行单选项；不可用时整行置灰并带原因。 */
function RadioOption({
  group,
  label,
  active,
  disabled,
  reason,
  icon: Icon,
  onSelect,
}: RadioOptionProps) {
  return (
    <label
      className={`find-similar-kind${active ? " is-active" : ""}${
        disabled ? " is-disabled" : ""
      }`}
      title={reason}
    >
      <input
        type="radio"
        name={group}
        checked={active}
        disabled={disabled}
        onChange={onSelect}
      />
      {Icon ? <Icon size={13} /> : null}
      <span>{label}</span>
      {disabled && reason ? <em>{reason}</em> : null}
    </label>
  );
}

export function FindSimilarPanel({
  open,
  reference,
  context,
  initial,
  onClose,
  onStart,
}: FindSimilarPanelProps) {
  const [capabilities, setCapabilities] = useState<ImageCapabilities[] | null>(null);
  const [capabilitiesError, setCapabilitiesError] = useState<string | null>(null);
  const [pendingSemantic, setPendingSemantic] = useState<number | null>(null);
  const [pendingError, setPendingError] = useState<string | null>(null);
  const [kind, setKind] = useState<SimilarKind>("phash");
  const [featureMode, setFeatureMode] = useState<FeatureMode>("overall");
  const [thresholds, setThresholds] = useState<ThresholdMap>(defaultThresholds);
  const [limitText, setLimitText] = useState(String(DEFAULT_RESULTS));
  const [scope, setScope] = useState<ScopeChoice>("view");
  const [includeSubfolders, setIncludeSubfolders] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const contextRef = useRef(context);
  contextRef.current = context;
  const wasOpenRef = useRef(open);

  const idsKey = reference?.ids.join(",") ?? "";
  const average = (reference?.ids.length ?? 0) > 1;
  const availableKinds = average ? AVERAGE_KINDS : SINGLE_KINDS;

  useEffect(() => {
    if (!open || !reference) return;
    let disposed = false;
    setCapabilities(null);
    setCapabilitiesError(null);
    void libraryApi
      .getImageCapabilities(reference.ids)
      .then((next) => {
        if (!disposed) setCapabilities(next);
      })
      .catch((cause: unknown) => {
        if (!disposed) setCapabilitiesError(errorMessage(cause));
      });
    return () => {
      disposed = true;
    };
  }, [open, idsKey, context]);

  // 新模式要等全库补齐；面板每次打开都重新读一次，回填完成后不用重开面板。
  useEffect(() => {
    if (!open) return;
    let disposed = false;
    setPendingSemantic(null);
    setPendingError(null);
    void libraryApi
      .getSemanticFeaturePending()
      .then((next) => {
        if (!disposed) setPendingSemantic(next.remaining);
      })
      .catch((cause: unknown) => {
        if (!disposed) setPendingError(errorMessage(cause));
      });
    return () => {
      disposed = true;
    };
  }, [open, context]);

  useEffect(() => {
    const current = contextRef.current;
    if (!open || !current) return;
    const availability = scopeAvailability(current);
    const preferred = initial?.scope.type ?? defaultScopeChoice(current, "view");
    setScope(
      availability[preferred as ScopeChoice]?.enabled
        ? (preferred as ScopeChoice)
        : availability.view.enabled
          ? "view"
          : "library",
    );
    setIncludeSubfolders(
      initial?.scope.type === "folder" ? initial.scope.includeSubfolders : true,
    );
    setKind(
      initial?.kind && availableKinds.includes(initial.kind)
        ? initial.kind
        : average
          ? "color8"
          : "phash",
    );
    // 多张参考图没有定义 SPP / MaxSim 的平均，退回「整体」。
    setFeatureMode(
      !average && initial?.mode && FEATURE_MODES.includes(initial.mode)
        ? initial.mode
        : "overall",
    );
    const nextThresholds = defaultThresholds();
    if (initial) {
      // pHash 的“不限”会以 256 传给 Rust；重新打开时要还原成未勾选。
      const unlimited =
        initial.threshold == null ||
        (initial.kind === "phash" && initial.threshold >= PHASH_UNLIMITED);
      nextThresholds[initial.kind] = {
        limited: !unlimited,
        value: unlimited
          ? nextThresholds[initial.kind].value
          : (initial.threshold as number),
      };
    }
    setThresholds(nextThresholds);
    setLimitText(String(initial?.limit ?? DEFAULT_RESULTS));
    setError(null);
    // 只在打开或参考图变化时重置；计数刷新不打断用户草稿。
  }, [open, idsKey, initial]);

  // 数据变化后某个范围不再可用：退回当前视图或整个图库，草稿保持不动。
  useEffect(() => {
    const wasOpen = wasOpenRef.current;
    wasOpenRef.current = open;
    // 刚打开的那次由重置逻辑决定范围，避免用上一轮的旧 scope 覆盖。
    if (!open || !context || !wasOpen) return;
    const availability = scopeAvailability(context);
    if (availability[scope]?.enabled) return;
    setScope(availability.view.enabled ? "view" : "library");
  }, [context, open, scope]);

  const capabilityMap = useMemo(() => {
    const map = new Map<number, ImageCapabilities>();
    for (const item of capabilities ?? []) map.set(item.imageId, item);
    return map;
  }, [capabilities]);

  const missingReferenceIds = useMemo(
    () =>
      reference
        ? reference.ids.filter((id) => capabilities && !capabilityMap.has(id))
        : [],
    [capabilityMap, capabilities, reference],
  );

  const metricAvailability = useMemo(() => {
    const map = {} as Record<SimilarKind, { enabled: boolean; reason?: string }>;
    if (!reference || !capabilities) {
      for (const value of SINGLE_KINDS) {
        map[value] = {
          enabled: false,
          reason: capabilitiesError ?? "正在读取参考图能力",
        };
      }
      return map;
    }
    if (missingReferenceIds.length > 0) {
      const reason = `有 ${missingReferenceIds.length} 张参考图已不存在或不在图库中`;
      for (const value of SINGLE_KINDS) map[value] = { enabled: false, reason };
      return map;
    }
    const missing = (field: "phash" | "colors" | "embedding" | "pose") =>
      reference.ids.filter((id) => !capabilityMap.get(id)?.[field]).length;
    const missingPhash = missing("phash");
    const missingColors = missing("colors");
    const missingEmbedding = missing("embedding");
    const missingPose = missing("pose");
    map.phash =
      missingPhash === 0
        ? { enabled: true }
        : { enabled: false, reason: `${missingPhash} 张参考图缺少 pHash 特征` };
    map.color4 = map.color8 =
      missingColors === 0
        ? { enabled: true }
        : { enabled: false, reason: `${missingColors} 张参考图缺少主色数据` };
    map.embedding =
      missingEmbedding === 0
        ? { enabled: true }
        : {
            enabled: false,
            reason: `${missingEmbedding} 张参考图还没有语义特征`,
          };
    map.pose =
      missingPose === 0
        ? { enabled: true }
        : {
            enabled: false,
            reason: `${missingPose} 张参考图还没有姿势数据`,
          };
    return map;
  }, [capabilities, capabilitiesError, capabilityMap, missingReferenceIds, reference]);

  /** 三种口径的可用性：参考图本身有该特征，且全库没有待补齐的图。 */
  const modeAvailability = useMemo(() => {
    const unavailable = (reason: string) => ({ enabled: false, reason });
    const map = {} as Record<FeatureMode, { enabled: boolean; reason?: string }>;
    if (!reference || !capabilities) {
      const reason = capabilitiesError ?? "正在读取参考图能力";
      for (const value of FEATURE_MODES) map[value] = unavailable(reason);
      return map;
    }
    if (missingReferenceIds.length > 0) {
      const reason = `有 ${missingReferenceIds.length} 张参考图已不存在或不在图库中`;
      for (const value of FEATURE_MODES) map[value] = unavailable(reason);
      return map;
    }
    const embedding = metricAvailability.embedding;
    map.overall = embedding.enabled
      ? { enabled: true }
      : unavailable(embedding.reason ?? "语义特征当前不可用");
    if (!embedding.enabled) {
      map.subject = unavailable(embedding.reason ?? "语义特征当前不可用");
      map.composition = unavailable(embedding.reason ?? "语义特征当前不可用");
      return map;
    }
    const missing = (field: "spp" | "maxsim") =>
      reference.ids.filter((id) => !capabilityMap.get(id)?.[field]).length;
    const gate = (field: "spp" | "maxsim", label: string) => {
      if (average) return unavailable(`${label}只支持 1 张参考图`);
      const count = missing(field);
      if (count > 0) return unavailable(`${count} 张参考图还没有语义特征`);
      if (pendingError) return unavailable("暂时无法读取语义特征生成进度");
      if (pendingSemantic == null) return unavailable("正在读取语义特征生成进度");
      if (pendingSemantic > 0)
        return unavailable(`语义特征还在生成（剩余 ${pendingSemantic} 张）`);
      return { enabled: true };
    };
    map.subject = gate("maxsim", "偏重主体");
    map.composition = gate("spp", "偏重构图");
    return map;
  }, [
    average,
    capabilities,
    capabilitiesError,
    capabilityMap,
    metricAvailability,
    missingReferenceIds,
    pendingError,
    pendingSemantic,
    reference,
  ]);

  useEffect(() => {
    if (!open || !capabilities) return;
    if (!metricAvailability[kind]?.enabled) {
      const next = availableKinds.find((value) => metricAvailability[value].enabled);
      if (next) setKind(next);
    }
  }, [availableKinds, capabilities, kind, metricAvailability, open]);

  const scopeOptions = useMemo(() => {
    const base = scopeAvailability(context);
    if (context && reference) {
      const referenceIds = new Set(reference.ids);
      if (
        context.selectedIds.length > 0 &&
        context.selectedIds.every((id) => referenceIds.has(id))
      ) {
        base.selected = {
          enabled: false,
          reason: "当前选择就是参考图，换一组选择作为范围",
        };
      }
    }
    return base;
  }, [context, reference]);

  useEffect(() => {
    if (!open || !context) return;
    if (!scopeOptions[scope].enabled) {
      setScope(scopeOptions.view.enabled ? "view" : "library");
    }
  }, [context, open, scope, scopeOptions]);

  const limit = Number(limitText);
  const limitValid =
    Number.isInteger(limit) && limit >= MIN_RESULTS && limit <= MAX_RESULTS;
  const threshold = thresholds[kind];
  const percent = thresholdPercent(kind, threshold.value);
  const metricAvailable = metricAvailability[kind]?.enabled === true;
  const modeAvailable =
    kind !== "embedding" || modeAvailability[featureMode].enabled === true;
  const referenceCount = reference?.ids.length ?? 0;
  const canStart =
    reference != null &&
    context != null &&
    limitValid &&
    metricAvailable &&
    modeAvailable &&
    referenceCount <= MAX_SIMILAR_REFERENCE;

  const start = () => {
    if (!reference || !context) return;
    if (!limitValid) {
      setError(`结果数量必须在 ${MIN_RESULTS} 到 ${MAX_RESULTS} 之间`);
      return;
    }
    if (!metricAvailable) {
      setError(metricAvailability[kind]?.reason ?? "这个指标当前不可用");
      return;
    }
    if (!modeAvailable) {
      setError(modeAvailability[featureMode].reason ?? "这个模式当前不可用");
      return;
    }
    const builtScope = buildScope(scope, context, { includeSubfolders });
    if (!builtScope) {
      setError("所选范围没有可用的图片");
      return;
    }
    setError(null);
    const rawThreshold = threshold.limited
      ? kind === "phash"
        ? Math.round(Math.min(threshold.value, PHASH_SLIDER_MAX))
        : threshold.value
      : kind === "phash"
        ? PHASH_UNLIMITED
        : null;
    onStart({
      kind,
      mode: kind === "embedding" ? featureMode : "overall",
      targetIds: reference.ids,
      threshold: rawThreshold,
      limit,
      scope: builtScope,
      scopeLabel: scopeLabelOf(scope, context, includeSubfolders),
    });
  };

  const previewItems = reference?.items.slice(0, 4) ?? [];
  const hiddenThumbs = Math.max(
    0,
    (reference?.ids.length ?? 0) - previewItems.length,
  );

  if (!open || !reference) return null;

  return (
    <div className="filter-panel find-similar-panel" role="dialog" aria-label="相似">
      <header className="filter-panel-head">
        <div>
          <strong>相似</strong>
          <span>
            {average
              ? `${reference.ids.length} 张参考图（平均）`
              : previewItems[0]
                ? fileNameOf(previewItems[0].filePath)
                : "1 张参考图"}
          </span>
        </div>
        <div className="filter-panel-head-actions">
          <IconButton label="关闭相似面板" onClick={onClose}>
            <X size={15} />
          </IconButton>
        </div>
      </header>

      <div className="filter-panel-body">
        {capabilitiesError ? (
          <div className="filter-panel-error">
            <AlertTriangle size={14} />
            <span>{capabilitiesError}</span>
          </div>
        ) : null}
        {error ? (
          <div className="filter-panel-error">
            <AlertTriangle size={14} />
            <span>{error}</span>
          </div>
        ) : null}

        <section className="filter-section find-similar-reference">
          <div className="find-similar-thumbs">
            {previewItems.map((item) => (
              <LibraryThumbnail
                key={item.id}
                image={item}
                className="find-similar-thumb"
              />
            ))}
          </div>
          <div className="find-similar-reference-text">
            <span>
              {average
                ? "只比较主色与语义特征的平均"
                : previewItems[0]
                  ? fileNameOf(previewItems[0].filePath)
                  : "当前选中图片"}
              {hiddenThumbs > 0 ? ` · 另有 ${hiddenThumbs} 张` : ""}
            </span>
          </div>
        </section>

        <section className="filter-section">
          <h4>指标</h4>
          <div className="find-similar-kinds" role="radiogroup" aria-label="相似指标">
            {availableKinds.map((value) => {
              const option = metricAvailability[value];
              const Icon = METRIC_ICONS[value];
              return (
                <RadioOption
                  key={value}
                  group="find-similar-kind"
                  label={similarKindLabel(value, average)}
                  active={kind === value}
                  disabled={!option.enabled}
                  reason={option.reason}
                  icon={Icon}
                  onSelect={() => setKind(value)}
                />
              );
            })}
          </div>
        </section>

        {kind === "embedding" ? (
          <section className="filter-section">
            <div
              className="find-similar-kinds find-similar-modes"
              role="radiogroup"
              aria-label="语义特征模式"
            >
              {FEATURE_MODES.map((value) => {
                const option = modeAvailability[value];
                return (
                  <RadioOption
                    key={value}
                    group="find-similar-feature-mode"
                    label={FEATURE_MODE_LABELS[value]}
                    active={featureMode === value}
                    disabled={!option.enabled}
                    reason={option.reason}
                    onSelect={() => setFeatureMode(value)}
                  />
                );
              })}
            </div>
          </section>
        ) : null}

        <section className="filter-section">
          <div className="find-similar-section-head">
            <h4>{kind === "embedding" ? "最低整体相似度" : "最低相似度"}</h4>
            <label className="find-similar-unlimited">
              <input
                type="checkbox"
                checked={!threshold.limited}
                onChange={(event) =>
                  setThresholds((previous) => ({
                    ...previous,
                    [kind]: { ...previous[kind], limited: !event.target.checked },
                  }))
                }
              />
              <span>不限</span>
            </label>
          </div>
          <div className={`find-similar-threshold${threshold.limited ? "" : " is-off"}`}>
            <div className="find-similar-slider">
              <input
                type="range"
                min={0}
                max={100}
                step={1}
                value={threshold.limited ? percent : 0}
                disabled={!threshold.limited}
                aria-label={kind === "embedding" ? "最低整体相似度" : "最低相似度"}
                onChange={(event) =>
                  setThresholds((previous) => ({
                    ...previous,
                    [kind]: {
                      ...previous[kind],
                      value: percentThreshold(kind, Number(event.target.value)),
                    },
                  }))
                }
              />
              <div className="find-similar-scale">
                <span>宽松</span>
                <span>严格</span>
              </div>
            </div>
            <output>{threshold.limited ? `${percent}%` : "不限"}</output>
          </div>
        </section>

        <section className="filter-section">
          <h4>最多结果</h4>
          <div className="find-similar-limit">
            <input
              className="input"
              type="number"
              min={MIN_RESULTS}
              max={MAX_RESULTS}
              step={100}
              value={limitText}
              onChange={(event) => setLimitText(event.target.value)}
            />
            <span>张</span>
          </div>
        </section>

        <section className="filter-section">
          <h4>范围</h4>
          <ScopePicker
            name="find-similar-scope"
            value={scope}
            onChange={setScope}
            context={context}
            availability={scopeOptions}
            includeSubfolders={includeSubfolders}
            onIncludeSubfoldersChange={setIncludeSubfolders}
          />
        </section>
      </div>

      <footer className="filter-panel-foot">
        <span className="filter-panel-count">结果会替换当前网格</span>
        <Button size="sm" variant="ghost" onClick={onClose}>
          取消
        </Button>
        <Button
          size="sm"
          variant="primary"
          icon={<ScanSearch size={13} />}
          disabled={!canStart}
          onClick={start}
        >
          查找相似
        </Button>
      </footer>
    </div>
  );
}
