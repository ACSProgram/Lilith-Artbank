import { useCallback, useEffect, useState } from "react";
import { Pencil, Tags, Trash2, X } from "lucide-react";
import { Button, IconButton } from "../../shared/components/Button";
import { Modal } from "../../shared/components/Modal";
import { Spinner } from "../../shared/components/Spinner";
import { useNotices } from "../../app/noticeContext";
import { libraryApi } from "./api";
import type { TagInfo } from "./types";
import { errorMessage } from "../../shared/errors";

interface TagManagerDialogProps {
  open: boolean;
  onClose: () => void;
  onChanged: () => void;
}

/** 与 Rust `query::MAX_TAGS` 一致：管理器一次只取图片最多的这些标签，搜索在当前列表内进行。 */
const TAG_LIMIT = 500;

export function TagManagerDialog({ open, onClose, onChanged }: TagManagerDialogProps) {
  const { notify, confirm } = useNotices();
  const [tags, setTags] = useState<TagInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [editingId, setEditingId] = useState<number | null>(null);
  const [editingName, setEditingName] = useState("");
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setTags(await libraryApi.getTags(TAG_LIMIT));
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!open) return;
    void load();
  }, [open, load]);

  useEffect(() => {
    if (!open) {
      setQuery("");
      setEditingId(null);
      setBusy(false);
    }
  }, [open]);

  const run = async (operation: () => Promise<string>, success: string) => {
    if (busy) return;
    setBusy(true);
    try {
      const detail = await operation();
      notify(success, "success", detail);
      await load();
      onChanged();
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
      return `「${tag.name}」→「${editingName.trim()}」`;
    }, "标签已重命名");
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
    }, "标签已删除");
  };

  const needle = query.trim().toLowerCase();
  const visible = tags.filter((tag) =>
    needle ? tag.name.toLowerCase().includes(needle) : true,
  );

  return (
    <Modal
      open={open}
      title="标签管理"
      subtitle="重命名与删除会立即影响所有引用该标签的图片。"
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
        {tags.length >= TAG_LIMIT ? (
          <p className="dialog-hint">
            标签较多，列表只显示图片最多的 {TAG_LIMIT} 个，搜索也只在这个范围内。
          </p>
        ) : null}
      </section>
      {error ? <p className="dialog-empty dialog-error">{error}</p> : null}
      <section className="dialog-section">
        <h3>
          <Tags size={14} /> 全部标签
          {loading ? <Spinner size={12} /> : null}
        </h3>
        {visible.length === 0 ? (
          <p className="dialog-empty">
            {loading ? "正在载入…" : needle ? "没有匹配的标签" : "还没有标签"}
          </p>
        ) : (
          <div className="tag-manager-list">
            {visible.map((tag) => (
              <div key={tag.id} className="tag-manager-row">
                {editingId === tag.id ? (
                  <div className="dialog-inline tag-manager-edit">
                    <input
                      className="input"
                      value={editingName}
                      autoFocus
                      disabled={busy}
                      onChange={(event) => setEditingName(event.target.value)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") {
                          event.preventDefault();
                          rename(tag);
                        }
                        if (event.key === "Escape") setEditingId(null);
                      }}
                    />
                    <Button size="sm" loading={busy} onClick={() => rename(tag)}>
                      保存
                    </Button>
                    <IconButton label="取消重命名" onClick={() => setEditingId(null)}>
                      <X size={13} />
                    </IconButton>
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
                        }}
                      >
                        <Pencil size={13} />
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
              </div>
            ))}
          </div>
        )}
      </section>
    </Modal>
  );
}
