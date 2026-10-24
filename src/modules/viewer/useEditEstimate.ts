import { useCallback, useEffect, useRef, useState } from "react";
import { buildEditRequest, type GeometryState } from "./model";
import { viewerApi } from "./api";
import type { EditSizeEstimate, ViewerSource } from "./types";

interface Pending {
  request: ReturnType<typeof buildEditRequest>;
  generation: number;
}

/**
 * 输出体积预览：几何/格式/质量变化后 500ms 防抖。
 * 同一时间只跑一次编码，期间到达的新请求只保留最新一份；
 * 过期结果直接丢弃，相当于“新请求取消旧请求”。
 */
export function useEditEstimate(
  imageId: number,
  source: ViewerSource | null,
  geometry: GeometryState | null,
  enabled: boolean,
): { estimate: EditSizeEstimate | null; loading: boolean } {
  const [estimate, setEstimate] = useState<EditSizeEstimate | null>(null);
  const [loading, setLoading] = useState(false);
  const generation = useRef(0);
  const running = useRef(false);
  const pending = useRef<Pending | null>(null);

  const runPending = useCallback(async function run(): Promise<void> {
    if (running.current) return;
    const task = pending.current;
    if (!task) {
      setLoading(false);
      return;
    }
    pending.current = null;
    running.current = true;
    try {
      const result = await viewerApi.estimateEditSize(task.request);
      if (task.generation === generation.current) setEstimate(result);
    } catch {
      if (task.generation === generation.current) setEstimate(null);
    } finally {
      running.current = false;
      if (pending.current) void run();
      else setLoading(false);
    }
  }, []);

  useEffect(() => {
    generation.current += 1;
    if (!enabled || !source || !geometry) {
      pending.current = null;
      setEstimate(null);
      setLoading(false);
      return;
    }
    const current = generation.current;
    const request = buildEditRequest(imageId, source, geometry);
    setLoading(true);
    const timer = window.setTimeout(() => {
      pending.current = { request, generation: current };
      void runPending();
    }, 500);
    return () => window.clearTimeout(timer);
  }, [enabled, geometry, imageId, runPending, source]);

  return { estimate, loading };
}
