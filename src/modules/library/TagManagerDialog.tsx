import { useCallback, useEffect, useRef, useState } from "react";
import { Merge, Pencil, Tags, Trash2, X } from "lucide-react";
import { Button, IconButton } from "../../shared/components/Button";
import { Modal } from "../../shared/components/Modal";
import { Spinner } from "../../shared/components/Spinner";
import { TagSuggestions } from "../../shared/components/TagSuggestions";
import { filterTagSuggestions, tagSuggestionRequest } from "../../shared/tagNames";
import { useNotices } from "../../app/noticeContext";
import { libraryApi } from "./api";
import type { TagInfo } from "./types";
import { errorMessage } from "../../shared/errors";

interface TagManagerDialogProps {
  open: boolean;
  onClose: () => void;
  onChanged: (change: TagChange) => void;
}

export interface TagChange {
  tagId: number;
  action: "rename" | "delete" | "merge";
  /** action=merge 时保留的目标标签。 */
  targetTagId?: number;
}

/** 与 Rust `query::MAX_TAGS` 一致：无查询时浏览图片最多的这些标签。 */
const TAG_LIMIT = 500;
const SEARCH_LIMIT = 200;
const MERGE_LIMIT = 50;
const SUGGESTION_LIMIT = 12;

export function TagManagerDialog({ open, onClose, onChanged }: TagManagerDialogProps) {
  const { notify, confirm } = useNotices();
  const [tags, setTags] = useState<TagInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [searchResults, setSearchResults] = useState<TagInfo[] | null>(null);
  const [searchLoading, setSearchLoading] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);
  const [searchTruncated, setSearchTruncated] = useState(false);
  const [editingId, setEditingId] = useState<number | null>(null);
  const [editingName, setEditingName] = useState("");
  const [editTouched, setEditTouched] = useState(false);
  const [editSuggestions, setEditSuggestions] = useState<
    { name: string; imageCount?: number }[]
  >([]);
  const [editSuggestLoading, setEditSuggestLoading] = useState(false);
  const [editSuggestError, setEditSuggestError] = useState<string | null>(null);
  const [mergingId, setMergingId] = useState<number | null>(null);
  const [mergeQuery, setMergeQuery] = useState("");
  const [mergeResults, setMergeResults] = useState<TagInfo[] | null>(null);
  const [mergeLoading, setMergeLoading] = useState(false);
  const [mergeError, setMergeError] = useState<string | null>(null);
  const [mergeTruncated, setMergeTruncated] = useState(false);
  const [mergeRetry, setMergeRetry] = useState(0);
  const [refreshKey, setRefreshKey] = useState(0);
  const [busy, setBusy] = useState(false);

  const openRef = useRef(open);
  const loadSeq = useRef(0);
  const searchSeq = useRef(0);
  const editSeq = useRef(0);
  const mergeSeq = useRef(0);

  useEffect(() => {
    openRef.current = open;
    if (open) return;
    loadSeq.current += 1;
    searchSeq.current += 1;
    editSeq.current += 1;
    mergeSeq.current += 1;
    setQuery("");
    setSearchResults(null);
    setSearchLoading(false);
    setSearchError(null);
    setSearchTruncated(false);
    setLoading(false);
    setError(null);
    setEditingId(null);
    setEditingName("");
    setEditTouched(false);
    setEditSuggestions([]);
    setEditSuggestLoading(false);
    setEditSuggestError(null);
    setMergingId(null);
    setMergeQuery("");
    setMergeResults(null);
    setMergeLoading(false);
    setMergeError(null);
    setMergeTruncated(false);
    setBusy(false);
  }, [open]);

  const load = useCallback(async () => {
    const seq = ++loadSeq.current;
    setLoading(true);
    setError(null);
    try {
      const next = await libraryApi.getTags(TAG_LIMIT);
      if (!openRef.current || seq !== loadSeq.current) return;
      setTags(next);
    } catch (cause) {
      if (!openRef.current || seq !== loadSeq.current) return;
      setError(errorMessage(cause));
    } finally {
      if (openRef.current && seq === loadSeq.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!open) return;
    void load();
  }, [open, load]);

  useEffect(() => {
    if (!open) return;
    const needle = query.trim();
    if (!needle) {
      searchSeq.current += 1;
      setSearchResults(null);
      setSearchLoading(false);
      setSearchError(null);
      setSearchTruncated(false);
      return;
    }
    const seq = ++searchSeq.current;
    setSearchLoading(true);
    setSearchError(null);
    const timer = window.setTimeout(() => {
      void libraryApi
        .searchTags(needle, SEARCH_LIMIT)
        .then((rows) => {
          if (!openRef.current || seq !== searchSeq.current) return;
          setSearchResults(rows);
          setSearchTruncated(rows.length >= SEARCH_LIMIT);
          setSearchLoading(false);
        })
        .catch((cause) => {
          if (!openRef.current || seq !== searchSeq.current) return;
          setSearchError(errorMessage(cause));
          setSearchLoading(false);
        });
    }, 160);
    return () => window.clearTimeout(timer);
  }, [open, query, refreshKey]);

  useEffect(() => {
    if (!open || editingId == null || !editTouched) {
      editSeq.current += 1;
      setEditSuggestions([]);
      setEditSuggestLoading(false);
      setEditSuggestError(null);
      return;
    }
    const request = tagSuggestionRequest(editingName);
    const seq = ++editSeq.current;
    setEditSuggestLoading(true);
    setEditSuggestError(null);
    const timer = window.setTimeout(() => {
      void libraryApi
        .searchTags(request.needle, MERGE_LIMIT)
        .then((rows) => {
          if (!openRef.current || seq !== editSeq.current) return;
          const items = rows
            .filter((tag) => tag.id !== editingId)
            .map(({ name, imageCount }) => ({ name, imageCount }));
          setEditSuggestions(
            filterTagSuggestions(items, [editingName], SUGGESTION_LIMIT),
          );
          setEditSuggestLoading(false);
        })
        .catch((cause) => {
          if (!openRef.current || seq !== editSeq.current) return;
          setEditSuggestions([]);
          setEditSuggestError(errorMessage(cause));
          setEditSuggestLoading(false);
        });
    }, request.debounceMs);
    return () => window.clearTimeout(timer);
  }, [open, editingId, editingName, editTouched]);

  useEffect(() => {
    if (!open || mergingId == null) {
      mergeSeq.current += 1;
      setMergeResults(null);
      setMergeLoading(false);
      setMergeError(null);
      setMergeTruncated(false);
      return;
    }
    const request = tagSuggestionRequest(mergeQuery);
    const seq = ++mergeSeq.current;
    setMergeLoading(true);
    setMergeError(null);
    const timer = window.setTimeout(() => {
      void libraryApi
        .searchTags(request.needle, MERGE_LIMIT)
        .then((rows) => {
          if (!openRef.current || seq !== mergeSeq.current) return;
          setMergeResults(rows);
          setMergeTruncated(rows.length >= MERGE_LIMIT);
          setMergeLoading(false);
        })
        .catch((cause) => {
          if (!openRef.current || seq !== mergeSeq.current) return;
          setMergeError(errorMessage(cause));
          setMergeLoading(false);
        });
    }, request.debounceMs);
    return () => window.clearTimeout(timer);
  }, [open, mergingId, mergeQuery, mergeRetry]);

  const run = async (operation: () => Promise<string>, success: string, change: TagChange) => {
    if (busy) return;
    setBusy(true);
    try {
      const detail = await operation();
      notify(success, "success", detail);
      await load();
      onChanged(change);
      setRefreshKey((key) => key + 1);
    } catch (cause) {
      notify("标签操作失败", "error", errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  const rename = (tag: TagInfo) => {
    if (!editingName.trim()) return;
    void run(async () => {
      await libraryApi.renameTag(tag.id, editingName);
      setEditingId(null);
      setEditingName("");
      setEditTouched(false);
      return `「${tag.name}」→「${editingName.trim()}」`;
    }, "标签已重命名", { tagId: tag.id, action: "rename" });
  };

  const remove = async (tag: TagInfo) => {
    const accepted = await confirm({
      title: `删除标签「${tag.name}」？`,
      message: `会从 ${tag.imageCount.toLocaleString()} 张图片上移除该标签，图片本身不会删除。`,
      confirmLabel: "删除标签",
      danger: true,
    });
    if (!accepted) return;
    void run(async () => {
      const affected = await libraryApi.deleteTag(tag.id);
      return `已从 ${affected.toLocaleString()} 张图片移除`;
    }, "标签已删除", { tagId: tag.id, action: "delete" });
  };

  const merge = async (source: TagInfo, target: TagInfo) => {
    setMergingId(null);
    setMergeQuery("");
    const accepted = await confirm({
      title: `把「${source.name}」合并到「${target.name}」？`,
      message: `${source.imageCount.toLocaleString()} 张图片会改挂到「${target.name}」，然后删除「${source.name}」这个标签。图片本身不会删除，可以在操作记录里撤销。`,
      confirmLabel: "合并标签",
      danger: true,
    });
    if (!accepted) return;
    void run(
      async () => {
        const report = await libraryApi.mergeTags(source.id, target.id);
        return `改挂 ${report.moved.toLocaleString()} 张，去重 ${report.duplicate.toLocaleString()} 张`;
      },
      "标签已合并",
      { tagId: source.id, action: "merge", targetTagId: target.id },
    );
  };

  const needle = query.trim();
  const visible = needle ? searchResults ?? [] : tags;
  const listBusy = loading || searchLoading;
  const truncationHint = needle
    ? searchTruncated
      ? `匹配较多，只显示图片最多的 ${SEARCH_LIMIT} 个，输入更多字符缩小范围。`
      : null
    : tags.length >= TAG_LIMIT
      ? `标签较多，列表只显示图片最多的 ${TAG_LIMIT} 个，搜索可找到不在列表里的标签。`
      : null;
  const mergeCandidates = (mergeResults ?? []).filter((tag) => tag.id !== mergingId);
  const mergeSuggestions = mergeCandidates.map(({ name, imageCount }) => ({
    name,
    imageCount,
  }));
  const mergePending =
    !mergeError && mergeSuggestions.length === 0 && (mergeResults === null || mergeLoading);
  const mergeEmpty =
    !mergeError && !mergeLoading && mergeResults !== null && mergeSuggestions.length === 0;

  return (
    <Modal
      open={open}
      title="标签管理"
      subtitle="重命名、删除与合并会立即影响所有引用该标签的图片。"
      width={560}
      onClose={onClose}
      footer={
        <Button variant="primary" onClick={onClose}>
          完成
        </Button>
      }
    >
      <section className="dialog-section">
        <input
          className="input"
          value={query}
          placeholder="查找标签"
          onChange={(event) => setQuery(event.target.value)}
        />
        {truncationHint ? <p className="dialog-hint">{truncationHint}</p> : null}
      </section>
      {error ? <p className="dialog-empty dialog-error">{error}</p> : null}
      {searchError ? (
        <p className="dialog-empty dialog-error">读取标签失败：{searchError}</p>
      ) : null}
      <section className="dialog-section">
        <h3>
          <Tags size={14} /> 全部标签
          {listBusy ? <Spinner size={12} /> : null}
        </h3>
        {visible.length === 0 ? (
          <p className="dialog-empty">
            {needle
              ? searchLoading
                ? "正在搜索…"
                : searchError
                  ? null
                  : "没有匹配的标签"
              : loading
                ? "正在载入…"
                : error
                  ? null
                  : "还没有标签"}
          </p>
        ) : (
          <div className="tag-manager-list">
            {visible.map((tag) => (
              <div key={tag.id} className="tag-manager-row">
                {editingId === tag.id ? (
                  <div className="tag-manager-edit">
                    <div className="dialog-inline">
                      <input
                        className="input"
                        value={editingName}
                        autoFocus
                        disabled={busy}
                        onChange={(event) => {
                          setEditingName(event.target.value);
                          setEditTouched(true);
                        }}
                        onKeyDown={(event) => {
                          if (event.key === "Enter") {
                            event.preventDefault();
                            rename(tag);
                          }
                          if (event.key === "Escape") setEditingId(null);
                        }}
                      />
                      {editSuggestLoading ? <Spinner size={12} /> : null}
                      <Button size="sm" loading={busy} onClick={() => rename(tag)}>
                        保存
                      </Button>
                      <IconButton
                        label="取消重命名"
                        onClick={() => {
                          setEditingId(null);
                          setEditTouched(false);
                        }}
                      >
                        <X size={13} />
                      </IconButton>
                    </div>
                    {editSuggestError ? (
                      <p className="dialog-empty dialog-error">建议读取失败</p>
                    ) : null}
                    {editTouched ? (
                      <TagSuggestions
                        inline
                        items={editSuggestions}
                        emptyText={editSuggestLoading ? "正在读取…" : undefined}
                        onPick={(name) => {
                          setEditingName(name);
                          setEditTouched(false);
                          setEditSuggestions([]);
                        }}
                      />
                    ) : null}
                  </div>
                ) : (
                  <>
                    <span className="tag-manager-name">{tag.name}</span>
                    <em>{tag.imageCount.toLocaleString()} 张</em>
                    <div className="tag-manager-actions">
                      <IconButton
                        label="重命名标签"
                        disabled={busy}
                        onClick={() => {
                          setEditingId(tag.id);
                          setEditingName(tag.name);
                          setEditTouched(false);
                          setEditSuggestions([]);
                          setEditSuggestError(null);
                        }}
                      >
                        <Pencil size={13} />
                      </IconButton>
                      <IconButton
                        label="合并到其它标签"
                        disabled={busy}
                        onClick={() => {
                          setMergingId(tag.id);
                          setMergeQuery("");
                        }}
                      >
                        <Merge size={13} />
                      </IconButton>
                      <IconButton
                        label="删除标签"
                        variant="danger"
                        disabled={busy}
                        onClick={() => void remove(tag)}
                      >
                        <Trash2 size={13} />
                      </IconButton>
                    </div>
                  </>
                )}
                {mergingId === tag.id ? (
                  <div className="tag-manager-merge">
                    <div className="dialog-inline">
                      <input
                        className="input"
                        value={mergeQuery}
                        autoFocus
                        placeholder="合并到哪个标签"
                        onChange={(event) => setMergeQuery(event.target.value)}
                        onKeyDown={(event) => {
                          if (event.key === "Escape") {
                            setMergingId(null);
                            setMergeQuery("");
                          }
                        }}
                      />
                      {mergeLoading ? <Spinner size={12} /> : null}
                    </div>
                    {mergeError ? (
                      <p className="dialog-empty dialog-error">
                        读取标签失败：{mergeError}{" "}
                        <button
                          type="button"
                          className="text-button"
                          onClick={() => setMergeRetry((value) => value + 1)}
                        >
                          重试
                        </button>
                      </p>
                    ) : null}
                    {mergePending ? <p className="dialog-empty">正在读取…</p> : null}
                    {mergeEmpty ? <p className="dialog-empty">没有可合并的标签</p> : null}
                    {mergeSuggestions.length > 0 ? (
                      <TagSuggestions
                        inline
                        items={mergeSuggestions}
                        onPick={(name) => {
                          const target = mergeCandidates.find(
                            (candidate) => candidate.name === name,
                          );
                          if (target) void merge(tag, target);
                        }}
                      />
                    ) : null}
                    {mergeTruncated ? (
                      <p className="dialog-hint">
                        只显示图片最多的 {MERGE_LIMIT} 个，输入更多字符缩小范围。
                      </p>
                    ) : null}
                  </div>
                ) : null}
              </div>
            ))}
          </div>
        )}
      </section>
    </Modal>
  );
}
