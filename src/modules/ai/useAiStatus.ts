import { useCallback, useEffect, useRef, useState } from "react";
import { aiApi } from "./api";
import type { AiStatus } from "./types";
import { errorMessage } from "../../shared/errors";

export interface AiStatusController {
  status: AiStatus | null;
  error: string | null;
  refresh: () => Promise<AiStatus | null>;
}

export function useAiStatus(enabled: boolean): AiStatusController {
  const [status, setStatus] = useState<AiStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const generation = useRef(0);
  const statusRef = useRef(status);
  statusRef.current = status;

  const refresh = useCallback(async () => {
    const current = generation.current + 1;
    generation.current = current;
    try {
      const next = await aiApi.getStatus();
      if (current !== generation.current) return null;
      setStatus(next);
      setError(null);
      return next;
    } catch (cause) {
      if (current !== generation.current) return null;
      setError(errorMessage(cause));
      return null;
    }
  }, []);

  useEffect(() => {
    if (!enabled) return;
    let disposed = false;
    void refresh();
    // 服务启动中需要快速轮询；任务进度走任务中心事件，不靠这里。
    const timer = window.setInterval(() => {
      if (disposed) return;
      if (statusRef.current?.process.state === "starting") void refresh();
    }, 1500);
    const slow = window.setInterval(() => {
      if (!disposed) void refresh();
    }, 8000);
    return () => {
      disposed = true;
      window.clearInterval(timer);
      window.clearInterval(slow);
    };
  }, [enabled, refresh]);

  return { status, error, refresh };
}
