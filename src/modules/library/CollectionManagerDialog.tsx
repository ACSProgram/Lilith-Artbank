import { useEffect, useState } from "react";
import { ArrowDown, ArrowUp, Layers, Pencil, Trash2, X } from "lucide-react";
import { Button, IconButton } from "../../shared/components/Button";
import { Modal } from "../../shared/components/Modal";
import { useNotices } from "../../app/noticeContext";
import { libraryApi } from "./api";
import type { CollectionInfo } from "./types";
import { errorMessage } from "../../shared/errors";

interface CollectionManagerDialogProps {
  open: boolean;
  collections: CollectionInfo[];
  onClose: () => void;
  onChanged: () => void;
}

/** 集合管理：重命名、排序、删除。新建在 `CreateCollectionDialog`，集合运算在独立对话框。 */
export function CollectionManagerDialog({
  open,
  collections,
  onClose,
  onChanged,
}: CollectionManagerDialogProps) {
  const { notify, confirm } = useNotices();
  const [editingId, setEditingId] = useState<number | null>(null);
  const [editingName, setEditingName] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) {
      setEditingId(null);
      setEditingName("");
      setBusy(false);
    }
  }, [open]);

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

  return (
    <Modal
      open={open}
      title="集合管理"
      width={620}
      onClose={onClose}
      footer={
        <Button variant="primary" onClick={onClose}>
          完成
        </Button>
      }
    >
      <section className="dialog-section">
        <h3>
          <Layers size={14} /> 集合列表
        </h3>
        {collections.length === 0 ? (
          <p className="dialog-empty">
            还没有集合。用视图栏的“新建集合”创建，再把选中的图片加进去。
          </p>
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
    </Modal>
  );
}
