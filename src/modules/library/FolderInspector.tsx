import type { ReactNode } from "react";
import { Folder, HardDrive, Images } from "lucide-react";
import { Spinner } from "../../shared/components/Spinner";
import { formatBytes } from "../../shared/format";
import { hasActiveFilters } from "./model";
import {
  DONUT_OTHER_KEY,
  donutSegments,
  scoreSegments,
  type DonutSegment,
} from "./folderStats";
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

  const extensions = donutSegments(data.extensions, 5);
  const scores = scoreSegments(data.scores);
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
              {extensions.length === 0 ? (
                <span className="inspector-empty">没有匹配的图片</span>
              ) : (
                <StatDonut segments={extensions} />
              )}
            </section>
            <section className="inspector-section">
              <h4>评分</h4>
              <StatDonut segments={scores} />
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

const DONUT_SIZE = 96;
const DONUT_RADIUS = 38;
const DONUT_STROKE = 12;
const DONUT_CIRCUMFERENCE = 2 * Math.PI * DONUT_RADIUS;

/** 合并出来的“其他”用灰段，其余按位置取分类色。 */
function segmentColor(segment: DonutSegment, index: number): string {
  if (segment.key === DONUT_OTHER_KEY) return "var(--chart-other)";
  return `var(--chart-${(index % 6) + 1})`;
}

/** 类型与评分的环图：分段 + 两列图例，中心是占比最大那一段。 */
function StatDonut({ segments }: { segments: DonutSegment[] }) {
  const sum = segments.reduce((acc, segment) => acc + segment.value, 0);
  const center = DONUT_SIZE / 2;
  const top = segments.reduce(
    (best, segment) => (segment.value > best.value ? segment : best),
    segments[0],
  );
  let passed = 0;
  const arcs = segments.map((segment, index) => {
    const length = sum > 0 ? (segment.value / sum) * DONUT_CIRCUMFERENCE : 0;
    const arc = { segment, index, length, offset: passed };
    passed += length;
    return arc;
  });

  return (
    <div className="folder-stat-donut">
      <svg
        viewBox={`0 0 ${DONUT_SIZE} ${DONUT_SIZE}`}
        role="img"
        aria-label={segments
          .map((segment) => `${segment.label} ${segment.value.toLocaleString()} 张`)
          .join("，")}
      >
        <circle
          className="folder-stat-donut-track"
          cx={center}
          cy={center}
          r={DONUT_RADIUS}
          strokeWidth={DONUT_STROKE}
        />
        {arcs.map(({ segment, index, length, offset }) =>
          length <= 0 ? null : (
            <circle
              key={segment.key}
              className="folder-stat-donut-arc"
              cx={center}
              cy={center}
              r={DONUT_RADIUS}
              strokeWidth={DONUT_STROKE}
              stroke={segmentColor(segment, index)}
              strokeDasharray={`${length} ${DONUT_CIRCUMFERENCE - length}`}
              strokeDashoffset={-offset}
              transform={`rotate(-90 ${center} ${center})`}
            />
          ),
        )}
        <text x={center} y={center - 2} textAnchor="middle" className="folder-stat-donut-value">
          {sum > 0 ? `${Math.round((top.value / sum) * 100)}%` : "—"}
        </text>
        <text x={center} y={center + 12} textAnchor="middle" className="folder-stat-donut-name">
          {sum > 0 ? top.label : ""}
        </text>
      </svg>

      <div className="folder-stat-donut-legend">
        {segments.map((segment, index) => (
          <div className="folder-stat-donut-item" key={segment.key}>
            <i style={{ background: segmentColor(segment, index) }} />
            <span title={segment.label}>{segment.label}</span>
            <em>
              {segment.value.toLocaleString()} ·{" "}
              {sum > 0 ? `${Math.round((segment.value / sum) * 100)}%` : "0%"}
            </em>
          </div>
        ))}
      </div>
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
