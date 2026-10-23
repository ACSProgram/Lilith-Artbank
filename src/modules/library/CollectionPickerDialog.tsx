import { useEffect, useState } from "react";
import { Layers, Plus } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { Modal } from "../../shared/components/Modal";
import { useNotices } from "../../app/noticeContext";
import { libraryApi } from "./api";
import type { CollectionInfo } from "./types";
import { errorMessage } from "../../shared/errors";

interface CollectionPickerDialogProps {
  open: boolean;
  imageIds: number[];
  collections: CollectionInfo[];
  onClose: () => void;
  onAdded: () => void;
}

export function CollectionPickerDialog({
  open,
  imageIds,
  collections,
  onClose,
  onAdded,
}: CollectionPickerDialogProps) {
  const { notify } = useNotices();
  const [newName, setNewName] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) {
      setNewName("");
      setBusy(false);
    }
  }, [open]);

  const addTo = async (collection: CollectionInfo) => {
    if (busy) return;
    setBusy(true);
    try {
      const added = await libraryApi.addImagesToCollection(collection.id, imageIds);
      notify(
        `已加入「${collection.name}」`,
        "success",
        added === imageIds.length
          ? `${added.toLocaleString()} 张`
          : `新增 ${added.toLocaleString()} 张，${(
              imageIds.length - added
            ).toLocaleString()} 张已存在`,
      );
      onAdded();
      onClose();
    } catch (cause) {
      notify("加入集合失败", "error", errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  const createAndAdd = async () => {
    if (busy || !newName.trim()) return;
    setBusy(true);
    try {
      const collection = await libraryApi.createCollection(newName);
      const added = await libraryApi.addImagesToCollection(collection.id, imageIds);
      notify(`已创建集合并加入 ${added.toLocaleString()} 张`, "success");
      setNewName("");
      onAdded();
      onClose();
    } catch (cause) {
      notify("创建集合并加入失败", "error", errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      title="加入集合"
      subtitle={`把选中的 ${imageIds.length.toLocaleString()} 张图片加入集合。`}
      width={480}
      onClose={onClose}
      footer={
        <Button variant="ghost" onClick={onClose}>
          取消
        </Button>
      }
    >
      <section className="dialog-section">
        <div className="dialog-inline">
          <input
            className="input"
            value={newName}
            placeholder="新建集合并加入"
            maxLength={120}
            disabled={busy}
            onChange={(event) => setNewName(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                void createAndAdd();
              }
            }}
          />
          <Button
            size="sm"
            variant="primary"
            icon={<Plus size={13} />}
            loading={busy}
            disabled={!newName.trim()}
            onClick={() => void createAndAdd()}
          >
            创建
          </Button>
        </div>
      </section>

      <section className="dialog-section">
        <h3>
          <Layers size={14} /> 现有集合
        </h3>
        {collections.length === 0 ? (
          <p className="dialog-empty">还没有集合，可以在上方直接新建。</p>
        ) : (
          <div className="collection-picker-list">
            {collections.map((collection) => (
              <button
                key={collection.id}
                type="button"
                className="collection-picker-row"
                disabled={busy}
                onClick={() => void addTo(collection)}
              >
                <Layers size={14} />
                <span>{collection.name}</span>
                <em>{collection.imageCount.toLocaleString()} 张</em>
              </button>
            ))}
          </div>
        )}
      </section>
    </Modal>
  );
}
