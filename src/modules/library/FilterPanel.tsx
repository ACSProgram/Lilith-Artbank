import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Tags, X } from "lucide-react";
import { Button, IconButton } from "../../shared/components/Button";
import { Spinner } from "../../shared/components/Spinner";
import { libraryApi } from "./api";
import {
  activeFilterCount,
  bytesToMbInput,
  dateInputValue,
  localDayEnd,
  localDayStart,
  mbToBytes,
  queryKey,
  toggleValue,
} from "./model";
import type {
  FeatureSummary,
  FilterCounts,
  LibraryFilters,
  LibraryQuery,
  PresenceFilter,
  TagInfo,
} from "./types";
import { presenceAvailability } from "../../shared/capability";
import { errorMessage } from "../../shared/errors";

interface FilterPanelProps {
  open: boolean;
  filters: LibraryFilters;
  query: LibraryQuery;
  dataVersion: number;
  /** 全库特征覆盖情况；为空时不做置灰。 */
  featureSummary?: FeatureSummary | null;
  onFiltersChange: (filters: LibraryFilters) => void;
  onClose: () => void;
  onReset: () => void;
  onManageTags: () => void;
}

export function FilterPanel({
  open,
  filters,
  query,
  dataVersion,
  featureSummary = null,
  onFiltersChange,
  onClose,
  onReset,
  onManageTags,
}: FilterPanelProps) {
  const [tags, setTags] = useState<TagInfo[]>([]);
  const [counts, setCounts] = useState<FilterCounts | null>(null);
  const [optionsLoading, setOptionsLoading] = useState(false);
  const [optionsError, setOptionsError] = useState<string | null>(null);
  const [countsLoading, setCountsLoading] = useState(false);
  const [countsError, setCountsError] = useState<string | null>(null);
  const [tagQuery, setTagQuery] = useState("");
  const [sizeMinDraft, setSizeMinDraft] = useState(bytesToMbInput(filters.sizeMin));
  const [sizeMaxDraft, setSizeMaxDraft] = useState(bytesToMbInput(filters.sizeMax));
  const countsGeneration = useRef(0);

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
  }, [open, loadOptions, dataVersion]);

  useEffect(() => {
    setSizeMinDraft(bytesToMbInput(filters.sizeMin));
    setSizeMaxDraft(bytesToMbInput(filters.sizeMax));
  }, [filters.sizeMin, filters.sizeMax]);

  // 计数只随筛选条件（countsKey）与数据版本变化重算；排序、翻页不触发。
  const countsKey = queryKey(query);

  useEffect(() => {
    if (!open) return;
    const generation = countsGeneration.current + 1;
    countsGeneration.current = generation;
    setCountsLoading(true);
    setCountsError(null);
    const timer = window.setTimeout(() => {
      void (async () => {
        try {
          const next = await libraryApi.getFilterCounts(query);
          if (generation !== countsGeneration.current) return;
          setCounts(next);
        } catch (cause) {
          if (generation !== countsGeneration.current) return;
          setCountsError(errorMessage(cause));
        } finally {
          if (generation === countsGeneration.current) setCountsLoading(false);
        }
      })();
    }, 160);
    return () => window.clearTimeout(timer);
  }, [open, countsKey, dataVersion]);

  useEffect(() => {
    if (!open) setTagQuery("");
  }, [open]);

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

  const extensions = counts
    ? counts.extensions.map((item) => item.value)
    : filters.extensions;

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
    return tags.filter((tag) => {
      if (keyword && !tag.name.toLowerCase().includes(keyword)) return false;
      if (!counts) return true;
      return filters.tagIds.includes(tag.id) || (tagCounts.get(tag.id) ?? 0) > 0;
    });
  }, [counts, filters.tagIds, tagCounts, tagQuery, tags]);

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
                  <em>{extensionCounts.get(extension) ?? "—"}</em>
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
          <h4>内容状态</h4>
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
                  onClick={() => patch({ tagMode: "any" })}
                >
                  任一
                </button>
                <button
                  type="button"
                  className={filters.tagMode === "all" ? "is-active" : ""}
                  onClick={() => patch({ tagMode: "all" })}
                >
                  全部
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
            {tagOptions.length === 0 ? (
              <span className="filter-loading">
                {optionsLoading || countsLoading ? <Spinner size={12} /> : null}
                {optionsLoading
                  ? "载入标签…"
                  : tagQuery.trim()
                    ? "没有匹配的标签"
                    : tags.length === 0
                      ? "还没有标签"
                      : "当前范围内没有带标签的图片"}
              </span>
            ) : (
              tagOptions.map((tag) => {
                const active = filters.tagIds.includes(tag.id);
                const count = counts ? (tagCounts.get(tag.id) ?? 0) : tag.imageCount;
                return (
                  <label key={tag.id} className={`filter-option${active ? " is-active" : ""}`}>
                    <input
                      type="checkbox"
                      checked={active}
                      onChange={() => patch({ tagIds: toggleValue(filters.tagIds, tag.id) })}
                    />
                    <span className="filter-option-label">{tag.name}</span>
                    <em>{count}</em>
                  </label>
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
