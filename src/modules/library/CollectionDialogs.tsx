import { useEffect, useRef, useState } from "react";
import { Layers, Plus } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { Field } from "../../shared/components/Field";
import { Modal } from "../../shared/components/Modal";
import { useNotices } from "../../app/noticeContext";
import { errorMessage } from "../../shared/errors";
import { libraryApi } from "./api";
import type { CollectionInfo, CombineMode } from "./types";

export type CollectionSource = "empty" | "selection" | "view" | "collection";

/** 预填名称：`集合 N`，与已有名称（不区分大小写）冲突时往后顺延。 */
export function suggestCollectionName(collections: CollectionInfo[]): string {
  const taken = new Set(collections.map((collection) => collection.name.trim().toLowerCase()));
  let index = collections.length + 1;
  while (taken.has(`集合 ${index}`.toLowerCase())) index += 1;
  return `集合 ${index}`;
}

interface CreateCollectionDialogProps {
  open: boolean;
  collections: CollectionInfo[];
  selectionIds: number[];
  /** “当前视图”这一项的称呼：普通视图是“当前视图”，相似结果是“相似结果”。 */
  viewLabel: string;
  /** 当前视图的图片数；还在统计时为 null。 */
  viewCount: number | null;
  /** 取当前视图的全部 id；只在用户选择“当前视图”时调用。 */
  resolveViewIds: () => Promise<number[]>;
  /** 从某个已有集合复制内容时，预置的来源集合。 */
  presetCollection?: CollectionInfo | null;
  /** 取指定集合的全部 id；只在用户选择“复制集合”时调用。 */
  resolveCollectionIds?: (collectionId: number) => Promise<number[]>;
  onCreated: (collection: CollectionInfo) => void;
  onClose: () => void;
}

/** 新建集合：预填名称 + 内容来源单选，回车即建。 */
export function CreateCollectionDialog({
  open,
  collections,
  selectionIds,
  viewLabel,
  viewCount,
  resolveViewIds,
  presetCollection = null,
  resolveCollectionIds,
  onCreated,
  onClose,
}: CreateCollectionDialogProps) {
  const { notify } = useNotices();
  const [name, setName] = useState("");
  const [source, setSource] = useState<CollectionSource>("empty");
  const [busy, setBusy] = useState(false);
  // 集合列表会在打开期间刷新（身份变化），用 ref 取最新值，避免把用户正在输入的名称重置掉。
  const collectionsRef = useRef(collections);
  collectionsRef.current = collections;

  useEffect(() => {
    if (!open) return;
    setName(suggestCollectionName(collectionsRef.current));
    setSource(presetCollection ? "collection" : "empty");
    setBusy(false);
  }, [open, presetCollection]);

  const create = async () => {
    const trimmed = name.trim();
    if (!trimmed || busy) return;
    setBusy(true);
    try {
      const collection = await libraryApi.createCollection(trimmed);
      const ids =
        source === "selection"
          ? selectionIds
          : source === "view"
            ? await resolveViewIds()
            : source === "collection" && presetCollection && resolveCollectionIds
              ? await resolveCollectionIds(presetCollection.id)
              : [];
      if (ids.length > 0) await libraryApi.addImagesToCollection(collection.id, ids);
      notify(
        `已创建集合「${collection.name}」`,
        "success",
        ids.length > 0 ? `包含 ${ids.length.toLocaleString()} 张图片` : "空集合",
      );
      onCreated(collection);
    } catch (cause) {
      notify("创建集合失败", "error", errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  const options: { value: CollectionSource; label: string; hint: string; disabled: boolean }[] = [
    { value: "empty", label: "空集合", hint: "创建后再往里加图片", disabled: false },
    {
      value: "selection",
      label: `当前选择（${selectionIds.length.toLocaleString()} 张）`,
      hint: "把现在选中的图片放进去",
      disabled: selectionIds.length === 0,
    },
    {
      value: "view",
      label: `${viewLabel}（${viewCount == null ? "统计中…" : `${viewCount.toLocaleString()} 张`}）`,
      hint: "按当前筛选条件全部写入",
      disabled: viewCount === 0,
    },
  ];

  if (presetCollection) {
    options.push({
      value: "collection",
      label: `集合「${presetCollection.name}」（${presetCollection.imageCount.toLocaleString()} 张）`,
      hint: "复制这个集合的内容",
      disabled: presetCollection.imageCount === 0 || !resolveCollectionIds,
    });
  }

  return (
    <Modal
      open={open}
      title="新建集合"
      subtitle="不会移动或复制文件。"
      width={460}
      dismissible={!busy}
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose} disabled={busy}>
            取消
          </Button>
          <Button
            variant="primary"
            icon={<Plus size={14} />}
            loading={busy}
            disabled={!name.trim()}
            onClick={() => void create()}
          >
            创建
          </Button>
        </>
      }
    >
      <div className="dialog-form">
        <Field label="名称" hint="名称不能重复。">
          <input
            className="input"
            value={name}
            maxLength={120}
            autoFocus
            onFocus={(event) => event.currentTarget.select()}
            disabled={busy}
            onChange={(event) => setName(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                void create();
              }
            }}
          />
        </Field>
        <div className="dialog-field">
          <span className="field-label">内容来源</span>
          <div className="filter-option-list">
            {options.map((option) => (
              <label
                key={option.value}
                className={`filter-option${source === option.value ? " is-active" : ""}${
                  option.disabled ? " is-disabled" : ""
                }`}
                title={option.disabled ? "当前没有可用图片" : option.hint}
              >
                <input
                  type="radio"
                  name="collection-source"
                  checked={source === option.value}
                  disabled={option.disabled || busy}
                  onChange={() => setSource(option.value)}
                />
                <span className="filter-option-label">{option.label}</span>
                <em>{option.hint}</em>
              </label>
            ))}
          </div>
        </div>
      </div>
    </Modal>
  );
}

interface RenameCollectionDialogProps {
  collection: CollectionInfo | null;
  onRenamed: (collection: CollectionInfo) => void;
  onClose: () => void;
}

export function RenameCollectionDialog({
  collection,
  onRenamed,
  onClose,
}: RenameCollectionDialogProps) {
  const { notify } = useNotices();
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const collectionId = collection?.id ?? null;
  const initialName = collection?.name ?? "";

  useEffect(() => {
    if (collectionId == null) return;
    setName(initialName);
    setBusy(false);
  }, [collectionId, initialName]);

  const rename = async () => {
    const trimmed = name.trim();
    if (!collection || !trimmed || busy) return;
    setBusy(true);
    try {
      await libraryApi.renameCollection(collection.id, trimmed);
      onRenamed({ ...collection, name: trimmed });
    } catch (cause) {
      notify("重命名失败", "error", errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={collection != null}
      title="重命名集合"
      width={420}
      dismissible={!busy}
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose} disabled={busy}>
            取消
          </Button>
          <Button variant="primary" loading={busy} disabled={!name.trim()} onClick={() => void rename()}>
            保存
          </Button>
        </>
      }
    >
      <div className="dialog-form">
        <Field label="名称">
          <input
            className="input"
            value={name}
            maxLength={120}
            autoFocus
            onFocus={(event) => event.currentTarget.select()}
            disabled={busy}
            onChange={(event) => setName(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                void rename();
              }
            }}
          />
        </Field>
      </div>
    </Modal>
  );
}

interface CombineCollectionsDialogProps {
  open: boolean;
  collections: CollectionInfo[];
  onCombined: () => void;
  onClose: () => void;
}

/** 集合运算：并集 / 交集 / 差集，结果存成新集合。 */
export function CombineCollectionsDialog({
  open,
  collections,
  onCombined,
  onClose,
}: CombineCollectionsDialogProps) {
  const { notify } = useNotices();
  const [mode, setMode] = useState<CombineMode>("union");
  const [ids, setIds] = useState<number[]>([]);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) return;
    setIds([]);
    setName("");
    setMode("union");
    setBusy(false);
  }, [open]);

  const combine = async () => {
    if (ids.length < 2 || !name.trim() || busy) return;
    setBusy(true);
    try {
      await libraryApi.combineCollections(mode, ids, name.trim());
      notify("集合运算完成，已创建新集合", "success");
      onCombined();
      onClose();
    } catch (cause) {
      notify("集合运算失败", "error", errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      title="集合运算"
      subtitle="并集去重；交集与差集以第一个集合为准。"
      width={520}
      dismissible={!busy}
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose} disabled={busy}>
            取消
          </Button>
          <Button
            variant="primary"
            loading={busy}
            disabled={ids.length < 2 || !name.trim()}
            onClick={() => void combine()}
          >
            生成集合
          </Button>
        </>
      }
    >
      <div className="dialog-form">
        <div className="collection-combine-list">
          {collections.map((collection) => (
            <label
              key={collection.id}
              className={`filter-option${ids.includes(collection.id) ? " is-active" : ""}`}
            >
              <input
                type="checkbox"
                checked={ids.includes(collection.id)}
                disabled={busy}
                onChange={() =>
                  setIds((previous) =>
                    previous.includes(collection.id)
                      ? previous.filter((value) => value !== collection.id)
                      : [...previous, collection.id],
                  )
                }
              />
              <span className="filter-option-label">{collection.name}</span>
              <em>{collection.imageCount.toLocaleString()}</em>
            </label>
          ))}
          {collections.length < 2 ? (
            <p className="dialog-empty">至少需要两个集合才能运算。</p>
          ) : null}
        </div>
        <div className="dialog-inline">
          <select
            className="input"
            value={mode}
            disabled={busy}
            onChange={(event) => setMode(event.target.value as CombineMode)}
          >
            <option value="union">并集</option>
            <option value="intersection">交集</option>
            <option value="difference">差集（第一个集合减去其余）</option>
          </select>
          <input
            className="input"
            value={name}
            placeholder="新集合名称"
            maxLength={120}
            disabled={busy}
            onChange={(event) => setName(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                void combine();
              }
            }}
          />
        </div>
        <div className="settings-note">
          <Layers size={13} />
          结果是一个新集合，原集合不变。
        </div>
      </div>
    </Modal>
  );
}
