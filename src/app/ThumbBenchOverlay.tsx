import { useCallback, useEffect, useRef, useState } from "react";
import { Activity, Gauge, Play, RefreshCw, X, Zap } from "lucide-react";
import { Button } from "../shared/components/Button";
import { appApi } from "./api";
import { benchThumbnailUrl } from "../shared/thumbUrl";
import type { ThumbBenchInfo, ThumbReadStats } from "./types";

interface LoadResult {
  id: number;
  mode: "cold" | "hot";
  token: number;
  elapsedMs: number;
  count: number;
  failures: number;
  transferBytes: number;
}

interface ThumbBenchOverlayProps {
  open: boolean;
  onClose: () => void;
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "未知错误";
}

function formatBytes(bytes: number): string {
  if (bytes >= 1048576) return `${(bytes / 1048576).toFixed(2)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}

function grade(result: LoadResult): "ok" | "warn" | "fail" {
  if (result.failures > 0) return "fail";
  if (result.elapsedMs <= 1000) return "ok";
  if (result.elapsedMs <= 3000) return "warn";
  return "fail";
}

function gradeLabel(result: LoadResult): string {
  if (result.failures > 0) return "加载失败";
  if (result.elapsedMs <= 1000) return "达标 ≤1s";
  if (result.elapsedMs <= 3000) return "仅达硬上限 ≤3s";
  return "超出硬上限";
}

/** 真实 WebView2 下的 150 张缩略图加载基准；仅在开发构建展示。 */
export function ThumbBenchOverlay({ open, onClose }: ThumbBenchOverlayProps) {
  const [info, setInfo] = useState<ThumbBenchInfo | null>(null);
  const [readStats, setReadStats] = useState<ThumbReadStats | null>(null);
  const [results, setResults] = useState<LoadResult[]>([]);
  const [preparing, setPreparing] = useState(false);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const gridRef = useRef<HTMLDivElement>(null);
  const tokenSeq = useRef(0);
  const activeToken = useRef(0);
  const resultSeq = useRef(0);

  const busy = preparing || running;

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !busy) {
        event.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [open, busy, onClose]);

  const runLoad = useCallback(
    async (mode: "cold" | "hot", source: ThumbBenchInfo | null = info) => {
      if (!source || !gridRef.current) return;
      const container = gridRef.current;
      const token = mode === "cold" ? ++tokenSeq.current : activeToken.current;
      if (mode === "cold") activeToken.current = token;

      setRunning(true);
      setError(null);
      container.replaceChildren();

      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));

      const start = performance.now();
      const images: HTMLImageElement[] = [];
      for (let index = 0; index < source.count; index += 1) {
        const image = new Image();
        image.decoding = "async";
        image.fetchPriority = "high";
        image.alt = "";
        image.src = benchThumbnailUrl(index, token);
        container.appendChild(image);
        images.push(image);
      }

      await Promise.allSettled(images.map((image) => image.decode()));
      await new Promise<void>((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
      );

      const elapsedMs = performance.now() - start;
      const failures = images.filter((image) => !image.complete || image.naturalWidth === 0).length;
      const transferBytes = performance
        .getEntriesByType("resource")
        .filter(
          (entry) =>
            entry.name.includes("/bench/") && entry.name.includes(`run=${token}`),
        )
        .reduce((sum, entry) => sum + ((entry as PerformanceResourceTiming).transferSize || 0), 0);

      resultSeq.current += 1;
      setResults((current) =>
        [
          ...current,
          {
            id: resultSeq.current,
            mode,
            token,
            elapsedMs,
            count: source.count,
            failures,
            transferBytes,
          },
        ].slice(-6),
      );
      setRunning(false);
    },
    [info],
  );

  const prepare = async () => {
    if (busy) return;
    setPreparing(true);
    setError(null);
    try {
      const next = await appApi.thumbBenchPrepare();
      setInfo(next);
      setReadStats(null);
      setResults([]);
      tokenSeq.current = 0;
      activeToken.current = 0;
      setPreparing(false);
      await runLoad("cold", next);
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setPreparing(false);
    }
  };

  const measureRead = async () => {
    if (busy || !info) return;
    setPreparing(true);
    setError(null);
    try {
      setReadStats(await appApi.thumbBenchMeasureRead());
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setPreparing(false);
    }
  };

  const close = () => {
    if (busy) return;
    setInfo(null);
    setReadStats(null);
    setResults([]);
    setError(null);
    void appApi.thumbBenchCleanup().catch(() => undefined);
    onClose();
  };

  if (!open) return null;

  const readP50 = readStats?.p50Ms ?? info?.readP50Ms;
  const readP95 = readStats?.p95Ms ?? info?.readP95Ms;

  return (
    <div className="thumb-bench-overlay" role="dialog" aria-modal="true" aria-label="缩略图基准">
      <header className="thumb-bench-toolbar">
        <div className="thumb-bench-heading">
          <span className="thumb-bench-mark">
            <Activity size={16} />
          </span>
          <div>
            <strong>缩略图基准</strong>
            <span>thumb:// · 150 张 · 512px WebP</span>
          </div>
        </div>
        <div className="thumb-bench-actions">
          <Button
            size="sm"
            variant="secondary"
            icon={<RefreshCw size={14} />}
            loading={preparing}
            disabled={running}
            onClick={() => void prepare()}
          >
            {info ? "重新生成" : "生成样本"}
          </Button>
          <Button
            size="sm"
            variant="primary"
            icon={<Play size={14} />}
            disabled={!info || busy}
            onClick={() => void runLoad("cold")}
          >
            冷加载
          </Button>
          <Button
            size="sm"
            variant="secondary"
            icon={<Zap size={14} />}
            disabled={!info || busy || activeToken.current === 0}
            onClick={() => void runLoad("hot")}
          >
            热加载
          </Button>
          {info ? (
            <Button
              size="sm"
              variant="ghost"
              icon={<Gauge size={14} />}
              disabled={busy}
              onClick={() => void measureRead()}
            >
              重测读取
            </Button>
          ) : null}
          <Button
            size="sm"
            variant="ghost"
            icon={<X size={14} />}
            disabled={busy}
            onClick={close}
          >
            关闭
          </Button>
        </div>
      </header>

      <div className="thumb-bench-meta">
        {info ? (
          <>
            <span>
              样本 <strong>{info.count}</strong> 张
            </span>
            <span>
              包大小 <strong>{formatBytes(info.thumbBytes)}</strong>
            </span>
            <span>
              生成 <strong>{info.buildMs.toFixed(0)} ms</strong>
            </span>
            <span>
              Rust 读取 p50 <strong>{readP50?.toFixed(2)} ms</strong> / p95{" "}
              <strong>{readP95?.toFixed(2)} ms</strong>
            </span>
          </>
        ) : (
          <span>生成 150 张确定性样本并写入临时缩略图包，然后测量真实 WebView2 的冷/热加载。</span>
        )}
      </div>

      {error ? <div className="thumb-bench-error">{error}</div> : null}

      <div className="thumb-bench-results">
        {running ? <span className="thumb-bench-running">正在加载 150 张缩略图…</span> : null}
        {results.map((result) => (
          <span
            key={result.id}
            className={`thumb-bench-result thumb-bench-result-${grade(result)}`}
          >
            {result.mode === "cold" ? "冷加载" : "热加载"} {result.elapsedMs.toFixed(0)} ms
            <em>{gradeLabel(result)}</em>
            <small>
              失败 {result.failures} · 传输 {result.transferBytes > 0 ? formatBytes(result.transferBytes) : "—"}
            </small>
          </span>
        ))}
      </div>

      <div className="thumb-bench-grid" ref={gridRef} />
    </div>
  );
}
