import { useCallback, useEffect, useRef, useState } from "react";
import { aiApi } from "../modules/ai/api";
import type { ImageRow } from "../modules/library/types";
import type { SemanticBrowseState } from "../modules/library/model";
import { errorMessage } from "../shared/errors";

const PAGE_SIZE = 150;
const RESULT_LIMIT = 2_000;

interface InternalState {
  active: boolean;
  text: string;
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
  text: "",
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

export function useSemanticSearch(): SemanticBrowseState {
  const [state, setState] = useState<InternalState>(INITIAL);
  const generation = useRef(0);
  const tokenRef = useRef<string | null>(null);
  const offsetRef = useRef<number | null>(null);
  const inFlight = useRef(false);

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
    setState(INITIAL);
  }, []);

  const search = useCallback((text: string) => {
    const trimmed = text.trim();
    if (!trimmed) return;
    const current = generation.current + 1;
    generation.current = current;
    tokenRef.current = null;
    offsetRef.current = null;
    inFlight.current = true;
    setState({
      ...INITIAL,
      active: true,
      text: trimmed,
      busy: true,
      status: "loading",
    });

    void (async () => {
      try {
        const result = await aiApi.semanticSearch(trimmed, RESULT_LIMIT);
        if (current !== generation.current) return;
        tokenRef.current = result.token;
        const page = await aiApi.getSimilarityPage<ImageRow>(result.token, 0, PAGE_SIZE);
        if (current !== generation.current) return;
        offsetRef.current = page.nextOffset;
        setState({
          active: true,
          text: trimmed,
          token: result.token,
          model: result.model,
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

  const retry = useCallback(() => {
    if (tokenRef.current) {
      const text = state.text;
      clear();
      search(text);
    } else if (state.text) {
      search(state.text);
    }
  }, [clear, search, state.text]);

  const ids = useCallback(async () => {
    const token = tokenRef.current;
    if (!token) return [];
    return aiApi.getSimilarityIds(token);
  }, []);

  return {
    ...state,
    search,
    clear,
    loadMore,
    retry,
    ids,
  };
}
