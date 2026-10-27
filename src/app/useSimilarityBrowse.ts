import { useCallback, useEffect, useRef, useState } from "react";
import { aiApi } from "../modules/ai/api";
import type { ImageRow } from "../modules/library/types";
import { PAGE_SIZE, type SimilarityBrowseState } from "../modules/library/model";
import type { FindSimilarConfig, SimilarKind } from "../shared/similarity";
import { errorMessage } from "../shared/errors";

const DEFAULT_RESULTS_LIMIT = 2_000;

type Request =
  | { type: "semantic"; text: string }
  | { type: "similar"; config: FindSimilarConfig };

interface InternalState {
  active: boolean;
  mode: "semantic" | "similar";
  text: string;
  kind: SimilarKind | null;
  targetIds: number[];
  scopeLabel: string;
  token: string | null;
  model: string;
  total: number;
  elapsedMs: number;
  busy: boolean;
  stage: string | null;
  error: string | null;
  items: ImageRow[];
  status: "loading" | "ready" | "error";
  moreStatus: "idle" | "loading" | "error";
  moreError: string | null;
  hasMore: boolean;
}

const INITIAL: InternalState = {
  active: false,
  mode: "semantic",
  text: "",
  kind: null,
  targetIds: [],
  scopeLabel: "",
  token: null,
  model: "",
  total: 0,
  elapsedMs: 0,
  busy: false,
  stage: null,
  error: null,
  items: [],
  status: "ready",
  moreStatus: "idle",
  moreError: null,
  hasMore: false,
};

export function useSimilarityBrowse(): SimilarityBrowseState {
  const [state, setState] = useState<InternalState>(INITIAL);
  const generation = useRef(0);
  const tokenRef = useRef<string | null>(null);
  const offsetRef = useRef<number | null>(null);
  const inFlight = useRef(false);
  const lastRequest = useRef<Request | null>(null);

  useEffect(
    () => () => {
      generation.current += 1;
    },
    [],
  );

  // 语义搜索的慢阶段在后端推进；把阶段名显示在加载态里，避免“加载完成”后再次卡住。
  useEffect(() => {
    if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      if (disposed) return;
      unlisten = await listen<{ stage: string }>("semantic-stage", (event) => {
        setState((previous) =>
          previous.busy && previous.mode === "semantic"
            ? { ...previous, stage: event.payload.stage }
            : previous,
        );
      });
    })();
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const clear = useCallback(() => {
    generation.current += 1;
    tokenRef.current = null;
    offsetRef.current = null;
    inFlight.current = false;
    lastRequest.current = null;
    setState(INITIAL);
  }, []);

  const run = useCallback((request: Request) => {
    const current = generation.current + 1;
    generation.current = current;
    tokenRef.current = null;
    offsetRef.current = null;
    inFlight.current = true;
    lastRequest.current = request;
    const labels = {
      mode: request.type,
      text: request.type === "semantic" ? request.text : "",
      kind: request.type === "similar" ? request.config.kind : null,
      targetIds: request.type === "similar" ? request.config.targetIds : [],
      scopeLabel: request.type === "similar" ? request.config.scopeLabel : "",
    };
    setState({ ...INITIAL, ...labels, active: true, busy: true, status: "loading" });

    void (async () => {
      try {
        const result =
          request.type === "semantic"
            ? await aiApi.semanticSearch(request.text, DEFAULT_RESULTS_LIMIT)
            : await aiApi.findSimilarImages({
                kind: request.config.kind,
                targetIds: request.config.targetIds,
                threshold: request.config.threshold,
                limit: request.config.limit,
                scope: request.config.scope,
              });
        if (current !== generation.current) return;
        tokenRef.current = result.token;
        const page = await aiApi.getSimilarityPage<ImageRow>(result.token, 0, PAGE_SIZE);
        if (current !== generation.current) return;
        offsetRef.current = page.nextOffset;
        setState({
          ...labels,
          active: true,
          token: result.token,
          model: "model" in result ? result.model : "",
          total: result.total,
          elapsedMs: result.elapsedMs,
          busy: false,
          stage: null,
          error: null,
          items: page.items,
          status: "ready",
          moreStatus: "idle",
          moreError: null,
          hasMore: page.nextOffset !== null,
        });
      } catch (cause) {
        if (current !== generation.current) return;
        setState((previous) => ({
          ...previous,
          busy: false,
          status: "error",
          error: errorMessage(cause),
        }));
      } finally {
        if (current === generation.current) inFlight.current = false;
      }
    })();
  }, []);

  const search = useCallback(
    (text: string) => {
      const trimmed = text.trim();
      if (!trimmed) return;
      run({ type: "semantic", text: trimmed });
    },
    [run],
  );

  const findSimilar = useCallback(
    (config: FindSimilarConfig) => {
      run({ type: "similar", config });
    },
    [run],
  );

  const loadMore = useCallback(() => {
    const token = tokenRef.current;
    const offset = offsetRef.current;
    if (!token || offset == null || inFlight.current) return;
    const current = generation.current;
    inFlight.current = true;
    setState((previous) => ({ ...previous, moreStatus: "loading", moreError: null }));

    void (async () => {
      try {
        const page = await aiApi.getSimilarityPage<ImageRow>(token, offset, PAGE_SIZE);
        if (current !== generation.current) return;
        offsetRef.current = page.nextOffset;
        setState((previous) => ({
          ...previous,
          items: [...previous.items, ...page.items],
          hasMore: page.nextOffset !== null,
          moreStatus: "idle",
        }));
      } catch (cause) {
        if (current !== generation.current) return;
        setState((previous) => ({
          ...previous,
          moreStatus: "error",
          moreError: errorMessage(cause),
        }));
      } finally {
        if (current === generation.current) inFlight.current = false;
      }
    })();
  }, []);

  // 只有整次搜索失败时才会走到重试；翻页失败由 moreStatus 走 loadMore。
  const retry = useCallback(() => {
    if (lastRequest.current) run(lastRequest.current);
  }, [run]);

  const ids = useCallback(async () => {
    const token = tokenRef.current;
    if (!token) return [];
    return aiApi.getSimilarityIds(token);
  }, []);

  const patchItem = useCallback((imageId: number, update: Partial<ImageRow>) => {
    setState((previous) => {
      const index = previous.items.findIndex((item) => item.id === imageId);
      if (index < 0) return previous;
      const items = previous.items.slice();
      items[index] = { ...items[index], ...update };
      return { ...previous, items };
    });
  }, []);

  return {
    ...state,
    search,
    findSimilar,
    clear,
    loadMore,
    retry,
    ids,
    patchItem,
  };
}
