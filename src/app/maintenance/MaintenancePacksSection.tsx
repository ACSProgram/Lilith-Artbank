import { Package, Play, Square } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { Spinner } from "../../shared/components/Spinner";
import { formatBytes } from "../../shared/format";
import {
  formatRelative,
  segmentGarbageRatio,
  type PackStats,
} from "../maintenanceTypes";
import type { TaskSnapshot } from "../taskTypes";
import { TaskProgress } from "./MaintenanceReport";

interface MaintenancePacksSectionProps {
  packStats: PackStats | null;
  packStatus: "idle" | "loading" | "ready" | "error";
  packError: string | null;
  reclaimable: boolean;
  packActive: boolean;
  packTask: TaskSnapshot | null;
  onStartPackCompact: () => void;
  onCancelPackCompact: () => void;
}

export function MaintenancePacksSection({
  packStats,
  packStatus,
  packError,
  reclaimable,
  packActive,
  packTask,
  onStartPackCompact,
  onCancelPackCompact,
}: MaintenancePacksSectionProps) {
  return (
    <>
      <section className="dialog-section">
        <h3>
          <Package size={14} /> 缩略图段
        </h3>
        {packStatus === "loading" ? <Spinner size={14} /> : null}
        {packError ? <p className="dialog-empty dialog-error">{packError}</p> : null}
        {packStats ? (
          <>
            <div className="maintenance-actions">
              <Button
                variant="secondary"
                icon={<Play size={13} />}
                disabled={packActive || !reclaimable}
                title={
                  reclaimable
                    ? "现在就回收，不等空闲条件"
                    : "没有超过阈值的段，回收不会释放空间"
                }
                onClick={onStartPackCompact}
              >
                立即回收
              </Button>
              {packActive ? (
                <Button
                  variant="danger"
                  icon={<Square size={13} />}
                  onClick={onCancelPackCompact}
                >
                  取消
                </Button>
              ) : null}
              <span className="maintenance-summary">
                共 {formatBytes(packStats.totalSize)} · 已用{" "}
                {formatBytes(packStats.totalUsed)} · 可回收{" "}
                {formatBytes(packStats.garbageBytes)}
              </span>
            </div>
            {packTask ? <TaskProgress task={packTask} /> : null}
            <p className="dialog-hint">
              {packStats.lastCompact
                ? `上次回收：${formatRelative(packStats.lastCompact.at)} · 释放 ${formatBytes(
                    packStats.lastCompact.freedBytes,
                  )}`
                : "还没有回收过。"}
              {reclaimable
                ? ` 垃圾率超过 ${(packStats.triggerRatio * 100).toFixed(0)}% 且空闲三分钟时自动回收。`
                : " 现在没有需要回收的段。"}
            </p>
            <div className="maintenance-table-wrap">
              <table className="maintenance-table">
                <thead>
                  <tr>
                    <th>段</th>
                    <th>文件</th>
                    <th>大小</th>
                    <th>已用</th>
                    <th>垃圾</th>
                    <th>垃圾率</th>
                  </tr>
                </thead>
                <tbody>
                  {packStats.segments.map((segment) => (
                    <tr key={segment.id}>
                      <td className="list-numeric">#{segment.id}</td>
                      <td className="maintenance-path" title={segment.fileName}>
                        {segment.fileName}
                      </td>
                      <td className="list-numeric">{formatBytes(segment.size)}</td>
                      <td className="list-numeric">{formatBytes(segment.used)}</td>
                      <td className="list-numeric">{formatBytes(segment.garbage)}</td>
                      <td className="list-numeric">
                        {(segmentGarbageRatio(segment) * 100).toFixed(1)}%
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </>
        ) : null}
      </section>

      <section className="dialog-section">
        <h3>特征包</h3>
        <p className="dialog-hint">特征包只有统计，没有回收按钮：它们只在整库重建时才会重写。</p>
        {packStats && packStats.features.length > 0 ? (
          <div className="maintenance-table-wrap">
            <table className="maintenance-table">
              <thead>
                <tr>
                  <th>类型</th>
                  <th>模型</th>
                  <th>维度</th>
                  <th>包</th>
                  <th>条数</th>
                  <th>已用</th>
                  <th>垃圾</th>
                </tr>
              </thead>
              <tbody>
                {packStats.features.map((pack) => (
                  <tr key={`${pack.kind}-${pack.model}-${pack.packName}`}>
                    <td>{pack.kind}</td>
                    <td className="maintenance-path" title={pack.model}>
                      {pack.model}
                    </td>
                    <td className="list-numeric">{pack.dim}</td>
                    <td className="maintenance-path" title={pack.packName}>
                      {pack.packName}
                    </td>
                    <td className="list-numeric">{pack.blobs.toLocaleString()}</td>
                    <td className="list-numeric">{formatBytes(pack.used)}</td>
                    <td className="list-numeric">{formatBytes(pack.garbage)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <p className="dialog-empty">还没有生成过特征。</p>
        )}
      </section>
    </>
  );
}
