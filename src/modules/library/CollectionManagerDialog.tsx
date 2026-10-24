import { useEffect, useState } from "react";
import { ArrowDown, ArrowUp, Layers, Pencil, Plus, Trash2, X } from "lucide-react";
import { Button, IconButton } from "../../shared/components/Button";
import { Modal } from "../../shared/components/Modal";
import { useNotices } from "../../app/noticeContext";
import { libraryApi } from "./api";
import type { CollectionInfo, CombineMode } from "./types";
import { errorMessage } from "../../shared/errors";

interface CollectionManagerDialogProps {
  open: boolean;
  collections: CollectionInfo[];
  currentLabel?: string;
  onCreateFromCurrent?: (name: string) => Promise<void>;
  onClose: () => void;
  onChanged: () => void;
}

export function CollectionManagerDialog({
  open,
  collections,
  currentLabel = "当前结果",
  onCreateFromCurrent,
  onClose,
  onChanged,
}: CollectionManagerDialogProps) {
  const { notify, confirm } = useNotices();
  const [newName, setNewName] = useState("");
  const [editingId, setEditingId] = useState<number | null>(null);
  const [editingName, setEditingName] = useState("");
  const [combineMode, setCombineMode] = useState<CombineMode>("union");
  const [combineIds, setCombineIds] = useState<number[]>([]);
  const [combineName, setCombineName] = useState("");
  const [currentName, setCurrentName] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) {
      setNewName("");
      setEditingId(null);
      setEditingName("");
      setCombineIds([]);
      setCombineName("");
      setCurrentName("");
      setBusy(false);
    }
  }, [open]);

  useEffect(() => {
    setCombineIds((previous) =>
      previous.filter((id) => collections.some((collection) => collection.id === id)),
    );
  }, [collections]);

  const run = async (operation: () => Promise<void>, success?: string) => {
    if (busy) return;
    setBusy(true);
    try {
      await operation();
      if (success) notify(success, "success");
      onChanged();
    } catch (cause) {
      notify("集合操作失败", "error", errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  const create = () => {
    if (!newName.trim()) return;
    void run(async () => {
      await libraryApi.createCollection(newName);
      setNewName("");
    }, "集合已创建");
  };

  const createFromCurrent = () => {
    if (!currentName.trim() || !onCreateFromCurrent) return;
    void run(async () => {
      await onCreateFromCurrent(currentName);
      setCurrentName("");
    }, `集合已从${currentLabel}创建`);
  };

  const rename = (collection: CollectionInfo) => {
    if (!editingName.trim()) return;
    void run(async () => {
      await libraryApi.renameCollection(collection.id, editingName);
      setEditingId(null);
    }, "集合已重命名");
  };

  const remove = async (collection: CollectionInfo) => {
    const accepted = await confirm({
      title: `删除集合「${collection.name}」？`,
      message: `集合内的 ${collection.imageCount.toLocaleString()} 张图片不会被删除，只会移除集合本身。`,
      confirmLabel: "删除集合",
      danger: true,
    });
    if (!accepted) return;
    void run(async () => {
      await libraryApi.deleteCollection(collection.id);
    }, "集合已删除");
  };

  const move = (index: number, delta: number) => {
    const next = [...collections];
    const target = index + delta;
    if (target < 0 || target >= next.length) return;
    [next[index], next[target]] = [next[target], next[index]];
    void run(async () => {
      await libraryApi.reorderCollections(next.map((collection) => collection.id));
    });
  };

  const toggleCombine = (id: number) => {
    setCombineIds((previous) =>
      previous.includes(id)
        ? previous.filter((value) => value !== id)
        : [...previous, id],
    );
  };

  const combine = () => {
    if (combineIds.length < 2 || !combineName.trim()) return;
    void run(async () => {
      await libraryApi.combineCollections(combineMode, combineIds, combineName);
      setCombineIds([]);
      setCombineName("");
    }, "集合运算完成并已创建新集合");
  };

  return (
    <Modal
      open={open}
      title="集合管理"
      subtitle="集合引用图片 id；图片编辑或换哈希后仍然保留。"
      width={680}
      onClose={onClose}
      footer={
        <Button variant="primary" onClick={onClose}>
          完成
        </Button>
      }
    >
      <section className="dialog-section">
        <h3>
          <Plus size={14} /> 新建集合
        </h3>
        <div className="dialog-inline">
          <input
            className="input"
            value={newName}
            placeholder="集合名称"
            maxLength={120}
            disabled={busy}
            onChange={(event) => setNewName(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                create();
              }
            }}
          />
          <Button
            size="sm"
            variant="primary"
            icon={<Plus size={13} />}
            loading={busy}
            disabled={!newName.trim()}
            onClick={create}
          >
            创建
          </Button>
        </div>
      </section>

      {onCreateFromCurrent ? (
        <section className="dialog-section">
          <h3>
            <Layers size={14} /> 从{currentLabel}创建
          </h3>
          <p className="dialog-hint">
            把当前结果的全部 id 写入新集合；不需要先滚动加载，也不会改变当前选择。
          </p>
          <div className="dialog-inline">
            <input
              className="input"
              value={currentName}
              placeholder="集合名称"
              maxLength={120}
              disabled={busy}
              onChange={(event) => setCurrentName(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  createFromCurrent();
                }
              }}
            />
            <Button
              size="sm"
              variant="primary"
              icon={<Plus size={13} />}
              loading={busy}
              disabled={!currentName.trim()}
              onClick={createFromCurrent}
            >
              创建
            </Button>
          </div>
        </section>
      ) : null}

      <section className="dialog-section">
        <h3>
          <Layers size={14} /> 集合列表
        </h3>
        {collections.length === 0 ? (
          <p className="dialog-empty">还没有集合。先创建一个，再把选中的图片加入集合。</p>
        ) : (
          <div className="collection-manager-list">
            {collections.map((collection, index) => (
              <div key={collection.id} className="collection-manager-row">
                {editingId === collection.id ? (
                  <div className="dialog-inline collection-manager-edit">
                    <input
                      className="input"
                      value={editingName}
                      autoFocus
                      disabled={busy}
                      onChange={(event) => setEditingName(event.target.value)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") {
                          event.preventDefault();
                          rename(collection);
                        }
                        if (event.key === "Escape") setEditingId(null);
                      }}
                    />
                    <Button size="sm" loading={busy} onClick={() => rename(collection)}>
                      保存
                    </Button>
                    <IconButton label="取消重命名" onClick={() => setEditingId(null)}>
                      <X size={13} />
                    </IconButton>
                  </div>
                ) : (
                  <>
                    <span className="collection-manager-name" title={collection.name}>
                      {collection.name}
                    </span>
                    <em>{collection.imageCount.toLocaleString()} 张</em>
                    <div className="collection-manager-actions">
                      <IconButton
                        label="上移"
                        disabled={busy || index === 0}
                        onClick={() => move(index, -1)}
                      >
                        <ArrowUp size={13} />
                      </IconButton>
                      <IconButton
                        label="下移"
                        disabled={busy || index === collections.length - 1}
                        onClick={() => move(index, 1)}
                      >
                        <ArrowDown size={13} />
                      </IconButton>
                      <IconButton
                        label="重命名"
                        disabled={busy}
                        onClick={() => {
                          setEditingId(collection.id);
                          setEditingName(collection.name);
                        }}
                      >
                        <Pencil size={13} />
                      </IconButton>
                      <IconButton
                        label="删除集合"
                        variant="danger"
                        disabled={busy}
                        onClick={() => void remove(collection)}
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

      <section className="dialog-section">
        <h3>
          <Layers size={14} /> 集合运算
        </h3>
        <p className="dialog-hint">
          选择至少两个集合，生成一个新集合。并集按选择顺序去重；交集与差集以第一个集合的顺序为准。
        </p>
        {collections.length < 2 ? (
          <p className="dialog-empty">至少需要两个集合才能运算。</p>
        ) : (
          <>
            <div className="collection-combine-list">
              {collections.map((collection) => (
                <label
                  key={collection.id}
                  className={`filter-option${combineIds.includes(collection.id) ? " is-active" : ""}`}
                >
                  <input
                    type="checkbox"
                    checked={combineIds.includes(collection.id)}
                    disabled={busy}
                    onChange={() => toggleCombine(collection.id)}
                  />
                  <span className="filter-option-label">{collection.name}</span>
                  <em>{collection.imageCount.toLocaleString()}</em>
                </label>
              ))}
            </div>
            <div className="dialog-inline">
              <select
                className="input"
                value={combineMode}
                disabled={busy}
                onChange={(event) => setCombineMode(event.target.value as CombineMode)}
              >
                <option value="union">并集</option>
                <option value="intersection">交集</option>
                <option value="difference">差集（第一个集合减去其余）</option>
              </select>
              <input
                className="input"
                value={combineName}
                placeholder="新集合名称"
                maxLength={120}
                disabled={busy}
                onChange={(event) => setCombineName(event.target.value)}
              />
              <Button
                size="sm"
                variant="primary"
                loading={busy}
                disabled={combineIds.length < 2 || !combineName.trim()}
                onClick={combine}
              >
                生成集合
              </Button>
            </div>
          </>
        )}
      </section>
    </Modal>
  );
}
