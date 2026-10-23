import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { X } from "lucide-react";
import { Button, IconButton } from "../../shared/components/Button";
import { Spinner } from "../../shared/components/Spinner";
import { libraryApi } from "./api";
import { activeFilterCount, folderLabel, queryKey, toggleValue } from "./model";
import type {
  FilterCounts,
  FolderInfo,
  LibraryFilters,
  LibraryQuery,
  TagInfo,
} from "./types";

interface FilterPanelProps {
  open: boolean;
  filters: LibraryFilters;
  query: LibraryQuery;
  onFiltersChange: (filters: LibraryFilters) => void;
  onClose: () => void;
  onReset: () => void;
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "未知错误";
}

export function FilterPanel({
  open,
  filters,
  query,
  onFiltersChange,
  onClose,
  onReset,
}: FilterPanelProps) {
  const [folders, setFolders] = useState<FolderInfo[]>([]);
  const [tags, setTags] = useState<TagInfo[]>([]);
  const [counts, setCounts] = useState<FilterCounts | null>(null);
  const [optionsLoading, setOptionsLoading] = useState(false);
  const [optionsError, setOptionsError] = useState<string | null>(null);
  const [countsLoading, setCountsLoading] = useState(false);
  const [countsError, setCountsError] = useState<string | null>(null);
  const [tagQuery, setTagQuery] = useState("");
  const countsGeneration = useRef(0);

  const loadOptions = useCallback(async () => {
    setOptionsLoading(true);
    setOptionsError(null);
    try {
      const [nextFolders, nextTags] = await Promise.all([
        libraryApi.getFolders(),
        libraryApi.getTags(),
      ]);
      setFolders(nextFolders);
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
  }, [open, loadOptions]);

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
  }, [open, countsKey]);

  useEffect(() => {
    if (!open) setTagQuery("");
  }, [open]);

  const folderCounts = useMemo(
    () => new Map((counts?.folders ?? []).map((item) => [item.folderId, item.count])),
    [counts],
  );
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

  const visibleTags = tags.filter((tag) =>
    tagQuery.trim() ? tag.name.toLowerCase().includes(tagQuery.trim().toLowerCase()) : true,
  );

  if (!open) return null;

  const patch = (update: Partial<LibraryFilters>) => {
    onFiltersChange({ ...filters, ...update });
  };

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
          <h4>文件夹</h4>
          <div className="filter-option-list">
            {optionsLoading && folders.length === 0 ? (
              <span className="filter-loading">
                <Spinner size={12} /> 载入文件夹…
              </span>
            ) : (
              folders.map((folder) => {
                const active = filters.folderIds.includes(folder.id);
                const count = counts ? (folderCounts.get(folder.id) ?? 0) : folder.imageCount;
                return (
                  <label key={folder.id} className={`filter-option${active ? " is-active" : ""}`}>
                    <input
                      type="checkbox"
                      checked={active}
                      onChange={() => patch({ folderIds: toggleValue(filters.folderIds, folder.id) })}
                    />
                    <span className="filter-option-label" title={folder.path}>
                      {folderLabel(folder.path)}
                    </span>
                    <em>{count}</em>
                  </label>
                );
              })
            )}
          </div>
        </section>

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

        <section className="filter-section">
          <h4>评分</h4>
          <div className="filter-chips">
            {[0, 1, 2, 3, 4, 5].map((score) => {
              const active = filters.scores.includes(score);
              return (
                <button
                  key={score}
                  type="button"
                  className={`filter-chip${active ? " is-active" : ""}`}
                  onClick={() => patch({ scores: toggleValue(filters.scores, score) })}
                >
                  {score === 0 ? "未评分" : "★".repeat(score)}
                  <em>{scoreCounts.get(String(score)) ?? "—"}</em>
                </button>
              );
            })}
          </div>
        </section>

        <section className="filter-section">
          <div className="filter-section-heading">
            <h4>标签</h4>
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
          </div>
          <input
            className="input filter-tag-search"
            value={tagQuery}
            placeholder="查找标签"
            onChange={(event) => setTagQuery(event.target.value)}
          />
          <div className="filter-option-list filter-tag-list">
            {visibleTags.length === 0 ? (
              <span className="filter-loading">
                {optionsLoading ? <Spinner size={12} /> : null}
                {optionsLoading ? "载入标签…" : tagQuery.trim() ? "没有匹配的标签" : "还没有标签"}
              </span>
            ) : (
              visibleTags.map((tag) => {
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
