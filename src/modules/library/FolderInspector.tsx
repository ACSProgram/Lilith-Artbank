import type { ReactNode } from "react";
import { Folder, HardDrive, Images } from "lucide-react";
import { Spinner } from "../../shared/components/Spinner";
import { formatBytes } from "../../shared/format";
import { hasActiveFilters } from "./model";
import { scoreBars, topValueCounts, type StatBarItem } from "./folderStats";
import { useStats } from "./StatsProvider";
import type { LibraryQuery, StatsTag } from "./types";

interface FolderInspectorProps {
  query: LibraryQuery;
}

export function FolderInspector({ query }: FolderInspectorProps) {
  const { data, status, error, retry } = useStats("folderDetail", query);

  if (data == null) {
    if (status === "error") {
      return (
        <div className="inspector-state inspector-state-error">
          <span>{error ?? "统计失败"}</span>
          <button type="button" className="text-button" onClick={retry}>
            重试
          </button>
        </div>
      );
    }
    return (
      <div className="inspector-state">
        <Spinner size={14} />
        <span>正在统计…</span>
      </div>
    );
  }

  const extensionBars = topValueCounts(data.extensions, 6);
  const scores = scoreBars(data.scores);
  const multipleFolders = query.filters.folderIds.length > 1;
  const emptyText =
    query.search.trim().length > 0 || hasActiveFilters(query.filters)
      ? "没有匹配的图片"
      : multipleFolders
        ? "所选文件夹没有图片"
        : "这个文件夹没有图片";
  const scopeNote = multipleFolders
    ? `已选 ${query.filters.folderIds.length} 个文件夹`
    : query.filters.folderDirect
      ? "只统计直接位于所选文件夹的图片"
      : null;
  const maxTagCount = data.tags.reduce((max, tag) => Math.max(max, tag.count), 0);

  return (
    <div className="inspector-body">
      <div className="folder-stats">
        <div className="folder-stats-summary">
          <FolderStatTile
            icon={<Images size={13} />}
            value={data.imageCount.toLocaleString()}
            label="图片"
          />
          <FolderStatTile
            icon={<Folder size={13} />}
            value={data.subfolderCount.toLocaleString()}
            label="子文件夹"
          />
          <FolderStatTile
            icon={<HardDrive size={13} />}
            value={formatBytes(data.totalBytes)}
            label="总大小"
          />
        </div>
        {scopeNote ? <div className="folder-stats-note">{scopeNote}</div> : null}
        {status === "refreshing" ? (
          <div className="folder-stats-refreshing">
            <Spinner size={11} /> 更新中
          </div>
        ) : null}
        {error ? (
          <div className="folder-stats-error">
            <span>{error}</span>
            <button type="button" className="text-button" onClick={retry}>
              重试
            </button>
          </div>
        ) : null}

        {data.imageCount === 0 ? (
          <div className="inspector-empty folder-stats-empty">{emptyText}</div>
        ) : (
          <>
            <section className="inspector-section">
              <h4>类型</h4>
              {extensionBars.length === 0 ? (
                <span className="inspector-empty">没有匹配的图片</span>
              ) : (
                <StatBars items={extensionBars} total={data.imageCount} />
              )}
            </section>
            <section className="inspector-section">
              <h4>评分</h4>
              <StatBars items={scores} total={data.imageCount} />
            </section>
            <section className="inspector-section">
              <h4>常见标签</h4>
              {data.tags.length === 0 ? (
                <span className="inspector-empty">没有标签</span>
              ) : (
                <StatTags tags={data.tags} max={maxTagCount} />
              )}
            </section>
          </>
        )}
      </div>
    </div>
  );
}

function FolderStatTile({
  icon,
  value,
  label,
}: {
  icon: ReactNode;
  value: string;
  label: string;
}) {
  return (
    <div className="folder-stat-tile">
      {icon}
      <strong>{value}</strong>
      <span>{label}</span>
    </div>
  );
}

function StatBars({ items, total }: { items: StatBarItem[]; total: number }) {
  return (
    <div className="folder-stat-rows">
      {items.map((item) => {
        const share = total > 0 ? item.count / total : 0;
        return (
          <div className="folder-stat-row" key={item.label}>
            <span className="folder-stat-label" title={item.label}>
              {item.label}
            </span>
            <span className="folder-stat-track">
              <span
                className="folder-stat-fill"
                style={{ width: `${Math.round(share * 100)}%` }}
              />
            </span>
            <span className="folder-stat-value">
              {item.count.toLocaleString()} · {Math.round(share * 100)}%
            </span>
          </div>
        );
      })}
    </div>
  );
}

function StatTags({ tags, max }: { tags: StatsTag[]; max: number }) {
  return (
    <div className="folder-stat-tag-list">
      {tags.map((tag) => (
        <div className="folder-stat-tag" key={tag.id}>
          <span className="folder-stat-label" title={tag.name}>
            {tag.name}
          </span>
          <span className="folder-stat-track">
            <span
              className="folder-stat-fill"
              style={{ width: `${max > 0 ? Math.round((tag.count / max) * 100) : 0}%` }}
            />
          </span>
          <span className="folder-stat-value">{tag.count.toLocaleString()}</span>
        </div>
      ))}
    </div>
  );
}
