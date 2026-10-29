import { useCallback, useEffect, useMemo, useState } from "react";
import { Check, Minus, Tags, X } from "lucide-react";
import { Button, IconButton } from "../../shared/components/Button";
import { Spinner } from "../../shared/components/Spinner";
import { libraryApi } from "./api";
import {
  activeFilterCount,
  bytesToMbInput,
  cycleTagFilter,
  dateInputValue,
  localDayEnd,
  localDayStart,
  mbToBytes,
  tagOptionState,
  toggleValue,
} from "./model";
import type {
  FeatureSummary,
  LibraryFilters,
  LibraryQuery,
  PresenceFilter,
  TagInfo,
} from "./types";
import { presenceAvailability } from "../../shared/capability";
import { errorMessage } from "../../shared/errors";
import { useStats } from "./StatsProvider";

/** 标签搜索一次取回的上限；与后端 `search_tags` 的 200 上限一致。 */
const TAG_SEARCH_LIMIT = 200;
/** 输入防抖：每敲一个字都查一次库没有必要。 */
const TAG_SEARCH_DEBOUNCE_MS = 180;

interface FilterPanelProps {
  open: boolean;
  filters: LibraryFilters;
  query: LibraryQuery;
  /** 标签选项版本；标签目录变化才递增。 */
  tagOptionsVersion: number;
  featureSummary?: FeatureSummary | null;
  /** 数据变化后正在重取覆盖率；旧值继续用，不闪空。 */
  featureSummaryLoading?: boolean;
  onFiltersChange: (filters: LibraryFilters) => void;
  onClose: () => void;
  onReset: () => void;
  onManageTags: () => void;
}

export function FilterPanel({
  open,
  filters,
  query,
  tagOptionsVersion,
  featureSummary = null,
  featureSummaryLoading = false,
  onFiltersChange,
  onClose,
  onReset,
  onManageTags,
}: FilterPanelProps) {
  const [tags, setTags] = useState<TagInfo[]>([]);
  const [optionsLoading, setOptionsLoading] = useState(false);
  const [optionsError, setOptionsError] = useState<string | null>(null);
  const [tagQuery, setTagQuery] = useState("");
  /** 有输入时的后端搜索结果；标签库超过一次能取回的数量后，靠它才找得到靠后的标签。 */
  const [tagSearchResults, setTagSearchResults] = useState<TagInfo[] | null>(null);
  const [tagSearchLoading, setTagSearchLoading] = useState(false);
  /** 搜索失败单独记：面板顶部的「重试」只重取标签列表，重跑不了搜索。 */
  const [tagSearchError, setTagSearchError] = useState<string | null>(null);
  const [tagSearchAttempt, setTagSearchAttempt] = useState(0);
  const [sizeMinDraft, setSizeMinDraft] = useState(bytesToMbInput(filters.sizeMin));
  const [sizeMaxDraft, setSizeMaxDraft] = useState(bytesToMbInput(filters.sizeMax));

  const countsStats = useStats("facets", query, open);
  const counts = countsStats.data;
  const countsLoading =
    countsStats.status === "loading" ||
    countsStats.status === "idle" ||
    countsStats.status === "refreshing";
  const countsError = countsStats.error;

  const loadOptions = useCallback(async () => {
    setOptionsLoading(true);
    setOptionsError(null);
    try {
      const nextTags = await libraryApi.getTags();
      setTags(nextTags);
    } catch (cause) {
      setOptionsError(errorMessage(cause));
    } finally {
      setOptionsLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!open) return;
    void loadOptions();
  }, [open, loadOptions, tagOptionsVersion]);

  useEffect(() => {
    setSizeMinDraft(bytesToMbInput(filters.sizeMin));
    setSizeMaxDraft(bytesToMbInput(filters.sizeMax));
  }, [filters.sizeMin, filters.sizeMax]);

  useEffect(() => {
    if (!open) setTagQuery("");
  }, [open]);

  // 输入即查后端：初始列表只是前若干个标签，靠后的标签只能搜出来。
  useEffect(() => {
    if (!open) {
      setTagSearchLoading(false);
      return;
    }
    const keyword = tagQuery.trim();
    if (!keyword) {
      setTagSearchResults(null);
      setTagSearchError(null);
      setTagSearchLoading(false);
      return;
    }
    let cancelled = false;
    setTagSearchLoading(true);
    setTagSearchError(null);
    const timer = window.setTimeout(() => {
      void libraryApi
        .searchTags(keyword, TAG_SEARCH_LIMIT)
        .then((next) => {
          if (!cancelled) setTagSearchResults(next);
        })
        .catch((cause: unknown) => {
          // 不能静默当成“没有匹配的标签”，否则用户会以为标签不存在。
          if (!cancelled) setTagSearchError(errorMessage(cause));
        })
        .finally(() => {
          if (!cancelled) setTagSearchLoading(false);
        });
    }, TAG_SEARCH_DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [tagQuery, open, tagSearchAttempt]);

  const extensionCounts = useMemo(
    () => new Map((counts?.extensions ?? []).map((item) => [item.value, item.count])),
    [counts],
  );
  const scoreCounts = useMemo(
    () => new Map((counts?.scores ?? []).map((item) => [item.value, item.count])),
    [counts],
  );
  const tagCounts = useMemo(
    () => new Map((counts?.tags ?? []).map((item) => [item.tagId, item.count])),
    [counts],
  );

  /**
   * 分面只给“当前范围内有内容”的扩展名；已选中的即使计数为 0 也必须留着，
   * 否则叠加别的条件后 chip 会消失，用户没法单独取消它。
   */
  const extensions = useMemo(() => {
    const values = counts ? counts.extensions.map((item) => item.value) : [];
    const known = new Set(values);
    return [...values, ...filters.extensions.filter((value) => !known.has(value))];
  }, [counts, filters.extensions]);

  /** 见过的标签：初始列表 + 搜索结果；已选但不在列表里的标签靠它保持可见。 */
  const knownTags = useMemo(() => {
    const map = new Map<number, TagInfo>();
    for (const tag of tags) map.set(tag.id, tag);
    for (const tag of tagSearchResults ?? []) map.set(tag.id, tag);
    return map;
  }, [tags, tagSearchResults]);

  /**
   * 分面只显示“有内容”的项：计数为 0 且未被选中的不渲染，已选中的即使为 0 也保留
   * （否则用户没法取消选择）。计数还没回来时按“未过滤”处理，避免打开面板瞬间闪一下。
   */
  const scoreValues = useMemo(() => {
    if (!counts) return [0, 1, 2, 3, 4, 5];
    const values = new Set(counts.scores.map((item) => Number(item.value)));
    for (const score of filters.scores) values.add(score);
    return [...values].sort((a, b) => a - b);
  }, [counts, filters.scores]);

  const tagOptions = useMemo(() => {
    const keyword = tagQuery.trim().toLowerCase();
    // 有输入时用后端结果，没输入时用初始列表；两者都还要过一遍范围计数。
    // 首次搜索还没回来前先用初始列表按关键字过滤，避免闪一下“没有匹配的标签”。
    const source = keyword ? (tagSearchResults ?? tags) : tags;
    const listed = source.filter((tag) => {
      if (keyword && !tag.name.toLowerCase().includes(keyword)) return false;
      if (!counts) return true;
      // “无标签”时不按计数过滤：搜索与集合视图的标签分面按命中集统计，会全是 0；
      // 仍列出全部标签，方便直接切回按标签筛选。
      if (filters.tagPresence === "missing") return true;
      return (
        filters.tagIds.includes(tag.id) ||
        filters.excludeTagIds.includes(tag.id) ||
        (tagCounts.get(tag.id) ?? 0) > 0
      );
    });
    // 已选中的标签必须始终可取消：不在当前列表里（例如超出初始数量）时补在末尾。
    const listedIds = new Set(listed.map((tag) => tag.id));
    const pinned: TagInfo[] = [];
    for (const tagId of [...filters.tagIds, ...filters.excludeTagIds]) {
      if (listedIds.has(tagId)) continue;
      const tag = knownTags.get(tagId);
      if (!tag) continue;
      listedIds.add(tagId);
      pinned.push(tag);
    }
    return pinned.length > 0 ? [...listed, ...pinned] : listed;
  }, [
    counts,
    filters.excludeTagIds,
    filters.tagIds,
    filters.tagPresence,
    knownTags,
    tagCounts,
    tagQuery,
    tagSearchResults,
    tags,
  ]);

  if (!open) return null;

  const patch = (update: Partial<LibraryFilters>) => {
    onFiltersChange({ ...filters, ...update });
  };

  const setImportedFrom = (value: string) => {
    const from = localDayStart(value);
    if (from == null) {
      patch({ importedFrom: null });
      return;
    }
    const to =
      filters.importedTo != null && filters.importedTo < from
        ? localDayEnd(value)
        : filters.importedTo;
    patch({ importedFrom: from, importedTo: to });
  };

  const setImportedTo = (value: string) => {
    const to = localDayEnd(value);
    if (to == null) {
      patch({ importedTo: null });
      return;
    }
    const from =
      filters.importedFrom != null && filters.importedFrom > to
        ? localDayStart(value)
        : filters.importedFrom;
    patch({ importedTo: to, importedFrom: from });
  };

  const applySize = () => {
    patch({
      sizeMin: mbToBytes(sizeMinDraft),
      sizeMax: mbToBytes(sizeMaxDraft),
    });
  };

  const presenceOptions: { value: PresenceFilter; label: string }[] = [
    { value: "any", label: "任意" },
    { value: "has", label: "有" },
    { value: "missing", label: "无" },
  ];

  return (
    <div className="filter-panel" role="dialog" aria-label="筛选图片">
      <header className="filter-panel-head">
        <div>
          <strong>筛选</strong>
          <span>
            {counts ? `${counts.total.toLocaleString()} 张匹配` : countsLoading ? "正在计算…" : "—"}
          </span>
        </div>
        <div className="filter-panel-head-actions">
          {countsLoading ? <Spinner size={13} /> : null}
          <IconButton label="关闭筛选" onClick={onClose}>
            <X size={15} />
          </IconButton>
        </div>
      </header>

      <div className="filter-panel-body">
        {optionsError ? (
          <div className="filter-panel-error">
            <span>{optionsError}</span>
            <button type="button" className="text-button" onClick={() => void loadOptions()}>
              重试
            </button>
          </div>
        ) : null}
        {countsError ? (
          <div className="filter-panel-error">
            <span>{countsError}</span>
            <button type="button" className="text-button" onClick={countsStats.retry}>
              重试
            </button>
          </div>
        ) : null}

        <section className="filter-section">
          <h4>扩展名</h4>
          <div className="filter-chips">
            {extensions.length === 0 ? (
              <span className="filter-loading">
                {countsLoading ? "正在统计扩展名…" : "无扩展名"}
              </span>
            ) : null}
            {extensions.map((extension) => {
              const active = filters.extensions.includes(extension);
              return (
                <button
                  key={extension}
                  type="button"
                  className={`filter-chip${active ? " is-active" : ""}`}
                  onClick={() => patch({ extensions: toggleValue(filters.extensions, extension) })}
                >
                  {extension.toUpperCase()}
                  <em>{extensionCounts.get(extension) ?? (counts ? 0 : "—")}</em>
                </button>
              );
            })}
          </div>
        </section>

        {scoreValues.length > 0 ? (
          <section className="filter-section">
            <h4>评分</h4>
            <div className="filter-chips">
              {scoreValues.map((score) => {
                const active = filters.scores.includes(score);
                return (
                  <button
                    key={score}
                    type="button"
                    className={`filter-chip${active ? " is-active" : ""}`}
                    onClick={() => patch({ scores: toggleValue(filters.scores, score) })}
                  >
                    {score === 0 ? "未评分" : "★".repeat(score)}
                    <em>{scoreCounts.get(String(score)) ?? 0}</em>
                  </button>
                );
              })}
            </div>
          </section>
        ) : null}

        <section className="filter-section">
          <h4>导入时间</h4>
          <div className="filter-range">
            <input
              type="date"
              className="input filter-date"
              value={dateInputValue(filters.importedFrom)}
              onChange={(event) => setImportedFrom(event.target.value)}
            />
            <span>至</span>
            <input
              type="date"
              className="input filter-date"
              value={dateInputValue(filters.importedTo)}
              onChange={(event) => setImportedTo(event.target.value)}
            />
          </div>
        </section>

        <section className="filter-section">
          <h4>文件大小（MB）</h4>
          <div className="filter-range">
            <input
              type="number"
              min={0}
              step={1}
              className="input filter-size"
              placeholder="最小"
              value={sizeMinDraft}
              onChange={(event) => setSizeMinDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  applySize();
                }
              }}
            />
            <span>至</span>
            <input
              type="number"
              min={0}
              step={1}
              className="input filter-size"
              placeholder="最大"
              value={sizeMaxDraft}
              onChange={(event) => setSizeMaxDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  applySize();
                }
              }}
            />
            <Button size="sm" variant="ghost" onClick={applySize}>
              应用
            </Button>
          </div>
        </section>

        <section className="filter-section">
          <div className="filter-section-heading">
            <h4>内容状态</h4>
            {featureSummaryLoading ? (
              <span className="filter-refreshing">更新中…</span>
            ) : null}
          </div>
          <div className="filter-presence">
            {[
              { key: "description" as const, label: "描述" },
              { key: "embedding" as const, label: "Embedding" },
              { key: "pose" as const, label: "Pose" },
            ].map((field) => {
              const availability = presenceAvailability(field.key, featureSummary);
              return (
                <div key={field.key} className="filter-presence-row">
                  <span>{field.label}</span>
                  <div className="filter-mode">
                    {presenceOptions.map((option) => {
                      const disabled = option.value === "has" && !availability.enabled;
                      return (
                        <button
                          key={option.value}
                          type="button"
                          className={filters[field.key] === option.value ? "is-active" : ""}
                          disabled={disabled}
                          title={disabled ? availability.reason : undefined}
                          onClick={() => patch({ [field.key]: option.value })}
                        >
                          {option.label}
                        </button>
                      );
                    })}
                  </div>
                </div>
              );
            })}
          </div>
        </section>

        <section className="filter-section">
          <div className="filter-section-heading">
            <h4>标签</h4>
            <div className="filter-section-tools">
              <div className="filter-mode">
                <button
                  type="button"
                  className={filters.tagMode === "any" ? "is-active" : ""}
                  disabled={filters.tagPresence === "missing"}
                  onClick={() => patch({ tagMode: "any" })}
                >
                  任一
                </button>
                <button
                  type="button"
                  className={filters.tagMode === "all" ? "is-active" : ""}
                  disabled={filters.tagPresence === "missing"}
                  onClick={() => patch({ tagMode: "all" })}
                >
                  全部
                </button>
                <button
                  type="button"
                  className={filters.tagPresence === "missing" ? "is-active" : ""}
                  onClick={() =>
                    patch(
                      filters.tagPresence === "missing"
                        ? { tagPresence: "any" }
                        : { tagPresence: "missing", tagIds: [] },
                    )
                  }
                >
                  无标签
                </button>
              </div>
              <button type="button" className="text-button" onClick={onManageTags}>
                <Tags size={11} /> 管理
              </button>
            </div>
          </div>
          <input
            className="input filter-tag-search"
            value={tagQuery}
            placeholder="查找标签"
            onChange={(event) => setTagQuery(event.target.value)}
          />
          <div className="filter-option-list filter-tag-list">
            {tagSearchError ? (
              <div className="filter-panel-error">
                <span>{tagSearchError}</span>
                <button
                  type="button"
                  className="text-button"
                  onClick={() => setTagSearchAttempt((value) => value + 1)}
                >
                  重试
                </button>
              </div>
            ) : tagOptions.length === 0 ? (
              <span className="filter-loading">
                {optionsLoading || countsLoading || tagSearchLoading ? <Spinner size={12} /> : null}
                {optionsLoading || tagSearchLoading
                  ? "载入标签…"
                  : tagQuery.trim()
                    ? "没有匹配的标签"
                    : tags.length === 0
                      ? "还没有标签"
                      : "当前范围内没有带标签的图片"}
              </span>
            ) : (
              tagOptions.map((tag) => {
                const state = tagOptionState(filters, tag.id);
                const count = counts ? (tagCounts.get(tag.id) ?? 0) : tag.imageCount;
                return (
                  <button
                    key={tag.id}
                    type="button"
                    className={`filter-option filter-option-tri${state === "include" ? " is-active" : ""}${
                      state === "exclude" ? " is-excluded" : ""
                    }`}
                    role="checkbox"
                    aria-checked={state === "neutral" ? false : state === "include" ? true : "mixed"}
                    title={
                      state === "include"
                        ? "已要求包含；再点一次改为排除"
                        : state === "exclude"
                          ? "已排除；再点一次取消"
                          : "点一次要求包含，再点一次排除"
                    }
                    onClick={() =>
                      onFiltersChange(cycleTagFilter(filters, tag.id))
                    }
                  >
                    <span className="filter-option-mark" aria-hidden="true">
                      {state === "include" ? <Check size={11} /> : null}
                      {state === "exclude" ? <Minus size={11} /> : null}
                    </span>
                    <span className="filter-option-label">{tag.name}</span>
                    <em>{count}</em>
                  </button>
                );
              })
            )}
          </div>
        </section>
      </div>

      <footer className="filter-panel-foot">
        <Button size="sm" variant="ghost" onClick={onReset} disabled={activeFilterCount(filters) === 0}>
          清除筛选
        </Button>
        <span className="filter-panel-count">已选 {activeFilterCount(filters)} 项</span>
        <Button size="sm" variant="primary" onClick={onClose}>
          完成
        </Button>
      </footer>
    </div>
  );
}
