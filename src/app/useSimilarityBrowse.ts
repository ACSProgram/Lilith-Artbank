import { useCallback, useEffect, useRef, useState } from "react";
import { aiApi } from "../modules/ai/api";
import type { ImageRow } from "../modules/library/types";
import { PAGE_SIZE, type SimilarityBrowseState } from "../modules/library/model";
import type { SimilarKind } from "../shared/similarity";
import { errorMessage } from "../shared/errors";

/** 与 Rust `library::similarity::DEFAULT_LIMIT` 保持一致。 */
const RESULTS_LIMIT = 2_000;

type Request =
  | { type: "semantic"; text: string }
  | { type: "similar"; kind: SimilarKind; targetId: number; threshold?: number };

interface InternalState {
  active: boolean;
  mode: "semantic" | "similar";
  text: string;
  kind: SimilarKind | null;
  targetId: number | null;
  token: string | null;
  model: string;
  total: number;
  elapsedMs: number;
  busy: boolean;
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
  targetId: null,
  token: null,
  model: "",
  total: 0,
  elapsedMs: 0,
  busy: false,
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
    const label =
      request.type === "semantic" ? request.text : `#${request.targetId}`;
    setState({
      ...INITIAL,
      active: true,
      mode: request.type,
      text: label,
      kind: request.type === "similar" ? request.kind : null,
      targetId: request.type === "similar" ? request.targetId : null,
      busy: true,
      status: "loading",
    });

    void (async () => {
      try {
        const result =
          request.type === "semantic"
            ? await aiApi.semanticSearch(request.text, RESULTS_LIMIT)
            : await aiApi.findSimilarImages(
                request.kind,
                request.targetId,
                request.threshold,
                RESULTS_LIMIT,
              );
        if (current !== generation.current) return;
        tokenRef.current = result.token;
        const page = await aiApi.getSimilarityPage<ImageRow>(result.token, 0, PAGE_SIZE);
        if (current !== generation.current) return;
        offsetRef.current = page.nextOffset;
        setState({
          active: true,
          mode: request.type,
          text: label,
          kind: request.type === "similar" ? request.kind : null,
          targetId: request.type === "similar" ? request.targetId : null,
          token: result.token,
          model: "model" in result ? result.model : "",
          total: result.total,
          elapsedMs: result.elapsedMs,
          busy: false,
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
    (kind: SimilarKind, targetId: number, threshold?: number) => {
      run({ type: "similar", kind, targetId, threshold });
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
