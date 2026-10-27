import { useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  Check,
  Copy,
  FileText,
  FolderInput,
  Hash,
  ImageOff,
  Layers,
  Palette,
  PanelRight,
  Plus,
  RotateCcw,
  Sparkles,
  Star,
  Tags,
  Trash2,
  X,
} from "lucide-react";
import { Button, IconButton } from "../../shared/components/Button";
import { FolderPickerDialog } from "../../shared/components/FolderPickerDialog";
import { Popover } from "../../shared/components/Popover";
import { Spinner } from "../../shared/components/Spinner";
import { copyText } from "../../shared/clipboard";
import { thumbnailUrl } from "../../shared/thumbUrl";
import { useNotices } from "../../app/noticeContext";
import { libraryApi } from "./api";
import {
  formatBytes,
  formatDate,
  formatDimensions,
  folderLabel,
} from "./model";
import type { ColorEntry, FolderInfo, ImageDetails } from "./types";
import type { DataChange } from "./dataChange";
import type { TagFilterAction, TagFilterScope } from "../../shared/tagFilter";
import { errorMessage } from "../../shared/errors";

interface InspectorPanelProps {
  selectedIds: number[];
  version: number;
  folders: FolderInfo[];
  currentCollectionId: number | null;
  /** 当前在临时视图：隐藏移动、集合与回收站操作。 */
  inTemp: boolean;
  onClose: () => void;
  onChanged: (change: DataChange) => void;
  onAddToCollection: () => void;
  onRemoveFromCollection: (ids: number[]) => void;
  onTrash: (ids: number[]) => void;
  onRestore: (ids: number[]) => void;
  onDelete: (ids: number[]) => void;
  /** 点击标签按选择的范围筛选；Ctrl/Cmd 点击直接在当前视图追加。 */
  onFilterTag?: (tagId: number, action: TagFilterAction, scope: TagFilterScope) => void;
  filterTagIds?: number[];
  /** 当前侧栏选中的文件夹；为空表示“当前文件夹”范围不可用。 */
  folderScopeLabel?: string | null;
  /** 当前集合；为空表示“当前集合”范围不可用。 */
  collectionScopeLabel?: string | null;
  onGenerateDescription?: (imageId: number) => Promise<{ text: string; model: string }>;
}

function descriptionSourceLabel(source: string): string {
  if (source === "ai") return "AI";
  if (source === "import") return "导入";
  return "手工";
}

export function InspectorPanel({
  selectedIds,
  version,
  folders,
  currentCollectionId,
  inTemp,
  onClose,
  onChanged,
  onAddToCollection,
  onRemoveFromCollection,
  onTrash,
  onRestore,
  onDelete,
  onFilterTag,
  filterTagIds = [],
  folderScopeLabel = null,
  collectionScopeLabel = null,
  onGenerateDescription,
}: InspectorPanelProps) {
  const single = selectedIds.length === 1 ? selectedIds[0] : null;
  const empty = selectedIds.length === 0;
  // 空选与单张都是“详情”，多选才是“批量编辑”。
  const detail = empty || single != null;
  return (
    <aside className="inspector" aria-label={detail ? "图片详情" : "批量编辑"}>
      <header className="inspector-head">
        <div>
          <strong>{detail ? "图片详情" : "批量编辑"}</strong>
          <span>
            {empty
              ? "未选择"
              : single
                ? `#${single}`
                : `已选择 ${selectedIds.length.toLocaleString()} 张`}
          </span>
        </div>
        <IconButton label="关闭侧栏" onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>
      {empty ? (
        <div className="inspector-placeholder">
          <PanelRight size={22} />
          <span>点选一张图片查看详情</span>
        </div>
      ) : single ? (
        <ImageInspector
          imageId={single}
          version={version}
          folders={folders}
          currentCollectionId={currentCollectionId}
          inTemp={inTemp}
          onChanged={onChanged}
          onAddToCollection={onAddToCollection}
          onRemoveFromCollection={() => onRemoveFromCollection([single])}
          onTrash={() => onTrash([single])}
          onRestore={() => onRestore([single])}
          onDelete={() => onDelete([single])}
          onFilterTag={onFilterTag}
          filterTagIds={filterTagIds}
          folderScopeLabel={folderScopeLabel}
          collectionScopeLabel={collectionScopeLabel}
          onGenerateDescription={onGenerateDescription}
        />
      ) : (
        <BatchInspector
          imageIds={selectedIds}
          folders={folders}
          currentCollectionId={currentCollectionId}
          inTemp={inTemp}
          onChanged={onChanged}
          onAddToCollection={onAddToCollection}
          onRemoveFromCollection={() => onRemoveFromCollection(selectedIds)}
          onTrash={() => onTrash(selectedIds)}
          onRestore={() => onRestore(selectedIds)}
          onDelete={() => onDelete(selectedIds)}
        />
      )}
    </aside>
  );
}

interface ImageInspectorProps {
  imageId: number;
  version: number;
  folders: FolderInfo[];
  currentCollectionId: number | null;
  inTemp: boolean;
  onChanged: (change: DataChange) => void;
  onAddToCollection: () => void;
  onRemoveFromCollection: () => void;
  onTrash: () => void;
  onRestore: () => void;
  onDelete: () => void;
  onFilterTag?: (tagId: number, action: TagFilterAction, scope: TagFilterScope) => void;
  filterTagIds: number[];
  folderScopeLabel: string | null;
  collectionScopeLabel: string | null;
  onGenerateDescription?: (imageId: number) => Promise<{ text: string; model: string }>;
}

function ImageInspector({
  imageId,
  version,
  folders,
  currentCollectionId,
  inTemp,
  onChanged,
  onAddToCollection,
  onRemoveFromCollection,
  onTrash,
  onRestore,
  onDelete,
  onFilterTag,
  filterTagIds,
  folderScopeLabel,
  collectionScopeLabel,
  onGenerateDescription,
}: ImageInspectorProps) {
  const { notify } = useNotices();
  const tagMenuAnchorRef = useRef<HTMLDivElement>(null);
  const [tagMenu, setTagMenu] = useState<{
    id: number;
    name: string;
    active: boolean;
  } | null>(null);
  const [detail, setDetail] = useState<ImageDetails | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [aiBusy, setAiBusy] = useState(false);
  const [thumbFailed, setThumbFailed] = useState(false);
  const [descriptionDraft, setDescriptionDraft] = useState("");
  const [tagDraft, setTagDraft] = useState("");
  const [tagSuggestions, setTagSuggestions] = useState<string[]>([]);
  const [folderDraft, setFolderDraft] = useState("");
  const [folderPickerOpen, setFolderPickerOpen] = useState(false);

  useEffect(() => {
    setTagMenu(null);
  }, [imageId]);

  useEffect(() => {
    let disposed = false;
    setLoading(true);
    setLoadError(null);
    void libraryApi
      .getImageDetail(imageId)
      .then((next) => {
        if (disposed) return;
        setDetail(next);
        setDescriptionDraft(next.description);
        setFolderDraft(next.folderPath);
        setThumbFailed(false);
      })
      .catch((cause: unknown) => {
        if (disposed) return;
        setLoadError(errorMessage(cause));
      })
      .finally(() => {
        if (!disposed) setLoading(false);
      });
    return () => {
      disposed = true;
    };
  }, [imageId, version]);

  useEffect(() => {
    const needle = tagDraft.trim();
    if (!needle) {
      setTagSuggestions([]);
      return;
    }
    let disposed = false;
    const timer = window.setTimeout(() => {
      void libraryApi
        .searchTags(needle, 12)
        .then((tags) => {
          if (disposed) return;
          const applied = new Set(detail?.tags.map((tag) => tag.name) ?? []);
          setTagSuggestions(tags.map((tag) => tag.name).filter((name) => !applied.has(name)));
        })
        .catch(() => {
          if (!disposed) setTagSuggestions([]);
        });
    }, 160);
    return () => {
      disposed = true;
      window.clearTimeout(timer);
    };
  }, [tagDraft, detail?.tags]);

  /** 单张写入：操作成功后返回它引起的变化，由图库统一决定谁失效。 */
  const run = useCallback(
    async (operation: () => Promise<DataChange | null>, success?: string) => {
      if (busy) return;
      setBusy(true);
      try {
        const change = await operation();
        if (success) notify(success, "success");
        if (change) onChanged(change);
      } catch (cause) {
        notify("操作失败", "error", errorMessage(cause));
      } finally {
        setBusy(false);
      }
    },
    [busy, notify, onChanged],
  );

  const saveDescription = () => {
    if (!detail || descriptionDraft.trim() === detail.description.trim()) return;
    void run(async () => {
      await libraryApi.updateImageDescription(detail.id, descriptionDraft);
      return {
        kind: "metadata",
        source: "local",
        reason: "description",
        ids: [detail.id],
        fields: ["description"],
        descriptionEmpty: descriptionDraft.trim() === "",
        similarity: "keep",
      };
    }, "描述已保存");
  };

  const generateDescription = async () => {
    if (!onGenerateDescription || !detail || aiBusy) return;
    setAiBusy(true);
    try {
      const result = await onGenerateDescription(detail.id);
      setDescriptionDraft(result.text);
      setDetail((previous) =>
        previous
          ? {
              ...previous,
              description: result.text,
              descriptionSource: "ai",
              descriptionModel: result.model,
            }
          : previous,
      );
      notify("AI 描述已生成", "success");
      onChanged({
        kind: "metadata",
        source: "local",
        reason: "aiDescription",
        ids: [detail.id],
        fields: ["description"],
        descriptionEmpty: result.text.trim() === "",
        similarity: "keep",
      });
    } catch (cause) {
      notify("AI 描述失败", "error", errorMessage(cause));
    } finally {
      setAiBusy(false);
    }
  };

  const setScore = (score: number) => {
    if (!detail || detail.score === score) return;
    void run(async () => {
      await libraryApi.updateImageScore(detail.id, score);
      setDetail((previous) => (previous ? { ...previous, score } : previous));
      return {
        kind: "metadata",
        source: "local",
        reason: "score",
        ids: [detail.id],
        fields: ["score"],
        score,
        similarity: "keep",
      };
    });
  };

  const addTag = (name: string) => {
    const trimmed = name.trim();
    if (!detail || !trimmed) return;
    if (detail.tags.some((tag) => tag.name.toLowerCase() === trimmed.toLowerCase())) {
      setTagDraft("");
      return;
    }
    void run(async () => {
      const applied = await libraryApi.setImageTags(detail.id, [
        ...detail.tags.map((tag) => tag.name),
        trimmed,
      ]);
      setDetail((previous) => (previous ? { ...previous, tags: applied } : previous));
      setTagDraft("");
      setTagSuggestions([]);
      return {
        kind: "metadata",
        source: "local",
        reason: "tagAdd",
        ids: [detail.id],
        fields: ["tags"],
        tagAction: "add",
        rowPatches: [{ id: detail.id, update: { tags: applied.map((tag) => tag.name) } }],
        similarity: "keep",
      };
    });
  };

  const removeTag = (name: string) => {
    if (!detail) return;
    void run(async () => {
      const applied = await libraryApi.setImageTags(
        detail.id,
        detail.tags.map((tag) => tag.name).filter((value) => value !== name),
      );
      setDetail((previous) => (previous ? { ...previous, tags: applied } : previous));
      return {
        kind: "metadata",
        source: "local",
        reason: "tagRemove",
        ids: [detail.id],
        fields: ["tags"],
        tagAction: "remove",
        rowPatches: [{ id: detail.id, update: { tags: applied.map((tag) => tag.name) } }],
        similarity: "keep",
      };
    });
  };

  const moveFolder = (targetPath?: string) => {
    if (!detail) return;
    const target = (targetPath ?? folderDraft)
      .replaceAll("\\", "/")
      .trim()
      .replace(/^\/+|\/+$/g, "");
    if (target === detail.folderPath) {
      notify("图片已在该文件夹", "info");
      return;
    }
    void run(async () => {
      const report = await libraryApi.moveImagesToFolder([detail.id], target);
      if (report.failures.length > 0) {
        throw new Error(report.failures[0].reason);
      }
      setFolderDraft(target);
      if (report.moved === 0) {
        notify("图片已在该文件夹", "info");
        return null;
      }
      notify("文件夹已更新", "success");
      return {
        kind: "folder",
        source: "local",
        reason: "moveImage",
        ids: [detail.id],
        fields: ["folder"],
        targetFolderPath: target,
        similarity: "keep",
      };
    });
  };

  const copy = async (text: string, label: string) => {
    try {
      await copyText(text);
      notify(`已复制${label}`, "success");
    } catch (cause) {
      notify("复制失败", "error", errorMessage(cause));
    }
  };

  const chooseTagFilter = (action: TagFilterAction, scope: TagFilterScope) => {
    if (!tagMenu || !onFilterTag) return;
    onFilterTag(tagMenu.id, action, scope);
    setTagMenu(null);
  };

  if (loading && !detail) {
    return (
      <div className="inspector-state">
        <Spinner size={18} />
        <span>正在读取详情…</span>
      </div>
    );
  }
  if (loadError || !detail) {
    return (
      <div className="inspector-state inspector-state-error">
        <AlertTriangle size={18} />
        <span>{loadError ?? "图片不存在"}</span>
      </div>
    );
  }

  const inCollection = currentCollectionId != null;
  return (
    <div className="inspector-body">
      <div className="inspector-preview">
        {thumbFailed ? (
          <span className="inspector-preview-fail">
            <ImageOff size={20} />
          </span>
        ) : (
          <img
            src={thumbnailUrl(detail.id, detail.thumbVersion)}
            alt=""
            draggable={false}
            onError={() => setThumbFailed(true)}
          />
        )}
        {detail.deletedAt != null ? <span className="trash-badge">回收站</span> : null}
      </div>

      <div className="inspector-meta">
        <span>{formatDimensions(detail.width, detail.height)}</span>
        <span>{detail.format.toUpperCase()}</span>
        <span>{formatBytes(detail.fileSize)}</span>
        <span>{detail.hasAlpha ? "含透明" : "无透明"}</span>
      </div>

      <section className="inspector-section">
        <h4>
          <Star size={12} /> 评分
        </h4>
        <div className="score-stars">
          {[1, 2, 3, 4, 5].map((score) => (
            <button
              key={score}
              type="button"
              className={detail.score >= score ? "is-on" : ""}
              title={`${score} 星`}
              aria-label={`${score} 星`}
              disabled={busy}
              onClick={() => setScore(score)}
            >
              ★
            </button>
          ))}
          <button
            type="button"
            className="score-clear"
            disabled={busy || detail.score === 0}
            onClick={() => setScore(0)}
          >
            清除
          </button>
        </div>
      </section>

      <section className="inspector-section">
        <div className="inspector-section-heading">
          <h4>
            <Tags size={12} /> 标签
          </h4>
          <span>{detail.tags.length} 个</span>
        </div>
        <div className="tag-chips" ref={tagMenuAnchorRef}>
          {detail.tags.map((tag) => {
            const active = filterTagIds.includes(tag.id);
            return (
              <span key={tag.id} className={`tag-chip${active ? " is-filtering" : ""}`}>
                {onFilterTag ? (
                  <button
                    type="button"
                    className="tag-chip-name"
                    title={
                      active
                        ? "点击可改筛选范围或取消；Ctrl 点击取消该标签"
                        : "点击选择筛选范围；Ctrl 点击直接叠加到当前视图"
                    }
                    onClick={(event) => {
                      if (event.ctrlKey || event.metaKey) {
                        onFilterTag(tag.id, "append", "view");
                        return;
                      }
                      setTagMenu({ id: tag.id, name: tag.name, active });
                    }}
                  >
                    {tag.name}
                  </button>
                ) : (
                  <span className="tag-chip-name">{tag.name}</span>
                )}
                <button
                  type="button"
                  aria-label={`移除标签 ${tag.name}`}
                  disabled={busy}
                  onClick={() => removeTag(tag.name)}
                >
                  <X size={10} />
                </button>
              </span>
            );
          })}
          {detail.tags.length === 0 ? <span className="inspector-empty">还没有标签</span> : null}
        </div>
        <Popover
          open={tagMenu != null}
          anchorRef={tagMenuAnchorRef}
          onClose={() => setTagMenu(null)}
          minWidth={220}
        >
          {tagMenu ? (
            <>
              <div className="menu-section-title">筛选「{tagMenu.name}」</div>
              {tagMenu.active ? (
                <button
                  type="button"
                  className="context-menu-item"
                  onClick={() => chooseTagFilter("remove", "view")}
                >
                  取消该标签筛选
                </button>
              ) : null}
              <button
                type="button"
                className="context-menu-item"
                onClick={() => chooseTagFilter("apply", "view")}
              >
                在当前视图中筛选
                <em>保留现有条件</em>
              </button>
              <button
                type="button"
                className="context-menu-item"
                disabled={!folderScopeLabel}
                title={folderScopeLabel ?? "先在左侧选择一个文件夹"}
                onClick={() => chooseTagFilter("apply", "folder")}
              >
                仅在当前文件夹中筛选
                <em>{folderScopeLabel ?? "未选择文件夹"}</em>
              </button>
              <button
                type="button"
                className="context-menu-item"
                disabled={!collectionScopeLabel}
                title={collectionScopeLabel ?? "当前不在集合视图"}
                onClick={() => chooseTagFilter("apply", "collection")}
              >
                仅在当前集合中筛选
                <em>{collectionScopeLabel ?? "当前不在集合"}</em>
              </button>
              <button
                type="button"
                className="context-menu-item"
                onClick={() => chooseTagFilter("apply", "library")}
              >
                在整个图库中筛选
                <em>清除文件夹与集合</em>
              </button>
            </>
          ) : null}
        </Popover>
        <div className="tag-input-wrap">
          <input
            className="input"
            value={tagDraft}
            placeholder="输入标签，回车添加"
            disabled={busy}
            onChange={(event) => setTagDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                addTag(tagDraft);
              }
            }}
          />
          <IconButton
            label="添加标签"
            disabled={busy || !tagDraft.trim()}
            onClick={() => addTag(tagDraft)}
          >
            <Plus size={13} />
          </IconButton>
          {tagSuggestions.length > 0 ? (
            <div className="tag-suggestions">
              {tagSuggestions.map((name) => (
                <button key={name} type="button" onClick={() => addTag(name)}>
                  {name}
                </button>
              ))}
            </div>
          ) : null}
        </div>
      </section>

      <section className="inspector-section">
        <div className="inspector-section-heading">
          <h4>
            <FileText size={12} /> 描述
          </h4>
          <span>{descriptionSourceLabel(detail.descriptionSource)}</span>
        </div>
        <textarea
          className="input inspector-textarea"
          value={descriptionDraft}
          placeholder="记录构图、角色、用途或来源…"
          disabled={busy}
          onChange={(event) => setDescriptionDraft(event.target.value)}
          onKeyDown={(event) => {
            if ((event.ctrlKey || event.metaKey) && event.key === "Enter") {
              event.preventDefault();
              saveDescription();
            }
          }}
        />
        <div className="inspector-inline-actions">
          <Button
            size="sm"
            variant="primary"
            icon={<Check size={13} />}
            loading={busy}
            disabled={descriptionDraft.trim() === detail.description.trim()}
            onClick={saveDescription}
          >
            保存描述
          </Button>
          {onGenerateDescription ? (
            <Button
              size="sm"
              variant="secondary"
              icon={<Sparkles size={13} />}
              loading={aiBusy}
              disabled={busy || detail.deletedAt != null}
              onClick={() => void generateDescription()}
            >
              AI 生成
            </Button>
          ) : null}
          <IconButton
            label="复制描述"
            disabled={!detail.description}
            onClick={() => void copy(detail.description, "描述")}
          >
            <Copy size={13} />
          </IconButton>
        </div>
      </section>

      {!inTemp ? (
        <section className="inspector-section">
          <div className="inspector-section-heading">
            <h4>
              <FolderInput size={12} /> 文件夹
            </h4>
            <span>{folderLabel(detail.folderPath)}</span>
          </div>
          <div className="inspector-inline-actions">
            <input
              className="input"
              value={folderDraft}
              list="inspector-folders"
              placeholder="根目录或 人物/速写"
              disabled={busy}
              onChange={(event) => setFolderDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  moveFolder();
                }
              }}
            />
            <Button size="sm" loading={busy} onClick={() => moveFolder()}>
              移动
            </Button>
            <IconButton
              label="从文件夹树选择"
              disabled={busy}
              onClick={() => setFolderPickerOpen(true)}
            >
              <FolderInput size={13} />
            </IconButton>
          </div>
          <datalist id="inspector-folders">
            {folders.map((folder) => (
              <option key={folder.id} value={folder.path} />
            ))}
          </datalist>
          <FolderPickerDialog
            open={folderPickerOpen}
            title="移动图片"
            subtitle="选择目标文件夹；图片会移动到该文件夹下。"
            folders={folders}
            selectedPath={detail.folderPath}
            onClose={() => setFolderPickerOpen(false)}
            onSelect={(target) => {
              setFolderPickerOpen(false);
              moveFolder(target.path);
            }}
          />
        </section>
      ) : null}

      <section className="inspector-section">
        <h4>
          <Palette size={12} /> 主色
        </h4>
        <ColorRow label="4 色" colors={detail.colors4} />
        <ColorRow label="8 色" colors={detail.colors8} />
      </section>

      <section className="inspector-section">
        <h4>文件</h4>
        <dl className="inspector-facts">
          <dt>导入</dt>
          <dd>{formatDate(detail.importedAt)}</dd>
          <dt>修改</dt>
          <dd>{formatDate(detail.modifiedAt)}</dd>
          <dt>文件夹</dt>
          <dd title={detail.folderPath}>{folderLabel(detail.folderPath)}</dd>
        </dl>
        <div className="inspector-inline-actions">
          <IconButton label="复制内容哈希" onClick={() => void copy(detail.contentHash, "内容哈希")}>
            <Hash size={13} />
          </IconButton>
          <IconButton label="复制文件路径" onClick={() => void copy(detail.filePath, "文件路径")}>
            <Copy size={13} />
          </IconButton>
          {!inTemp ? (
            <IconButton label="加入集合" onClick={onAddToCollection}>
              <Layers size={13} />
            </IconButton>
          ) : null}
        </div>
      </section>

      {inTemp ? (
        <section className="inspector-section">
          <span className="field-hint">临时图片关闭、切换图库或退出应用时会自动清空。</span>
        </section>
      ) : (
        <section className="inspector-section inspector-actions">
          {detail.deletedAt == null ? (
            <Button size="sm" variant="secondary" icon={<Trash2 size={13} />} onClick={onTrash}>
              移入回收站
            </Button>
          ) : (
            <>
              <Button size="sm" variant="primary" icon={<RotateCcw size={13} />} onClick={onRestore}>
                恢复
              </Button>
              <Button size="sm" variant="danger" icon={<Trash2 size={13} />} onClick={onDelete}>
                永久删除
              </Button>
            </>
          )}
          {inCollection ? (
            <Button size="sm" variant="ghost" icon={<Layers size={13} />} onClick={onRemoveFromCollection}>
              移出当前集合
            </Button>
          ) : null}
        </section>
      )}
    </div>
  );
}

function ColorRow({ label, colors }: { label: string; colors: ColorEntry[] }) {
  return (
    <div className="color-row">
      <span className="color-row-label">{label}</span>
      <div className="color-swatches">
        {colors.map((color, index) => (
          <span
            key={`${color.r}-${color.g}-${color.b}-${index}`}
            className="color-swatch"
            title={`rgb(${color.r}, ${color.g}, ${color.b}) · ${Math.round(
              (color.weight / 255) * 100,
            )}%`}
            style={{ background: `rgb(${color.r} ${color.g} ${color.b})` }}
          />
        ))}
      </div>
    </div>
  );
}

interface BatchInspectorProps {
  imageIds: number[];
  folders: FolderInfo[];
  currentCollectionId: number | null;
  inTemp: boolean;
  onChanged: (change: DataChange) => void;
  onAddToCollection: () => void;
  onRemoveFromCollection: () => void;
  onTrash: () => void;
  onRestore: () => void;
  onDelete: () => void;
}

function BatchInspector({
  imageIds,
  folders,
  currentCollectionId,
  inTemp,
  onChanged,
  onAddToCollection,
  onRemoveFromCollection,
  onTrash,
  onRestore,
  onDelete,
}: BatchInspectorProps) {
  const { notify } = useNotices();
  const [busy, setBusy] = useState(false);
  const [score, setScore] = useState("unset");
  const [tagAdd, setTagAdd] = useState("");
  const [tagRemove, setTagRemove] = useState("");
  const [folderDraft, setFolderDraft] = useState("");
  const [folderPickerOpen, setFolderPickerOpen] = useState(false);

  const count = imageIds.length;
  const run = async (
    operation: () => Promise<string>,
    fallback: string,
    change: () => DataChange,
  ) => {
    if (busy) return;
    setBusy(true);
    try {
      const detail = await operation();
      notify(fallback, "success", detail);
      onChanged(change());
    } catch (cause) {
      notify("批量操作失败", "error", errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  const applyScore = () => {
    if (score === "unset") return;
    const value = Number(score);
    void run(
      async () => {
        const changed = await libraryApi.setImagesScore(imageIds, value);
        return `${changed.toLocaleString()} 张已更新`;
      },
      "评分已更新",
      // 批量写入只返回数量；按新评分就地更新已加载行由图库侧完成。
      () => ({
        kind: "metadata",
        source: "local",
        reason: "batchScore",
        ids: imageIds,
        fields: ["score"],
        score: value,
        similarity: "keep",
      }),
    );
  };

  const splitTags = (value: string) =>
    value
      .split(/[,，;；\n]/)
      .map((item) => item.trim())
      .filter(Boolean);

  const applyAddTags = () => {
    const names = splitTags(tagAdd);
    if (names.length === 0) return;
    void run(
      async () => {
        const added = await libraryApi.addTagsToImages(imageIds, names);
        setTagAdd("");
        return `新增 ${added.toLocaleString()} 个标签关联`;
      },
      "标签已添加",
      () => ({
        kind: "metadata",
        source: "local",
        reason: "batchTagAdd",
        ids: imageIds,
        fields: ["tags"],
        tagAction: "add",
        similarity: "keep",
      }),
    );
  };

  const applyRemoveTags = () => {
    const names = splitTags(tagRemove);
    if (names.length === 0) return;
    void run(
      async () => {
        const removed = await libraryApi.removeTagsFromImages(imageIds, names);
        setTagRemove("");
        return `移除 ${removed.toLocaleString()} 个标签关联`;
      },
      "标签已移除",
      () => ({
        kind: "metadata",
        source: "local",
        reason: "batchTagRemove",
        ids: imageIds,
        fields: ["tags"],
        tagAction: "remove",
        similarity: "keep",
      }),
    );
  };

  const applyMove = (targetPath?: string) => {
    const target = (targetPath ?? folderDraft)
      .replaceAll("\\", "/")
      .trim()
      .replace(/^\/+|\/+$/g, "");
    if (target === "") {
      notify("请输入目标文件夹", "info", "要移动到根目录，请用下方的「移动到根目录」。");
      return;
    }
    void run(
      async () => {
        const report = await libraryApi.moveImagesToFolder(imageIds, target);
        if (report.failures.length > 0) {
          throw new Error(
            `${report.failures.length} 张移动失败：${report.failures[0].reason}`,
          );
        }
        setFolderDraft(target);
        return `已移动 ${report.moved.toLocaleString()} 张，跳过 ${report.skipped.toLocaleString()} 张`;
      },
      "文件夹已更新",
      () => ({
        kind: "folder",
        source: "local",
        reason: "batchMove",
        ids: imageIds,
        fields: ["folder"],
        targetFolderPath: target,
        similarity: "keep",
      }),
    );
  };

  const moveToRoot = () => {
    void run(
      async () => {
        const report = await libraryApi.moveImagesToFolder(imageIds, "");
        if (report.failures.length > 0) {
          throw new Error(
            `${report.failures.length} 张移动失败：${report.failures[0].reason}`,
          );
        }
        return `已移动 ${report.moved.toLocaleString()} 张，跳过 ${report.skipped.toLocaleString()} 张`;
      },
      "文件夹已更新",
      () => ({
        kind: "folder",
        source: "local",
        reason: "batchMove",
        ids: imageIds,
        fields: ["folder"],
        targetFolderPath: "",
        similarity: "keep",
      }),
    );
  };

  return (
    <div className="inspector-body">
      <div className="batch-summary">
        批量操作会作用于全部 {count.toLocaleString()} 张已选图片。
      </div>

      <section className="inspector-section">
        <h4>
          <Star size={12} /> 评分
        </h4>
        <div className="inspector-inline-actions">
          <select
            className="input"
            value={score}
            disabled={busy}
            onChange={(event) => setScore(event.target.value)}
          >
            <option value="unset">不修改</option>
            {[0, 1, 2, 3, 4, 5].map((value) => (
              <option key={value} value={value}>
                {value === 0 ? "清除评分" : `${value} 星`}
              </option>
            ))}
          </select>
          <Button size="sm" loading={busy} disabled={score === "unset"} onClick={applyScore}>
            应用
          </Button>
        </div>
      </section>

      <section className="inspector-section">
        <h4>
          <Tags size={12} /> 添加标签
        </h4>
        <div className="inspector-inline-actions">
          <input
            className="input"
            value={tagAdd}
            placeholder="用逗号分隔多个标签"
            disabled={busy}
            onChange={(event) => setTagAdd(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                applyAddTags();
              }
            }}
          />
          <Button size="sm" loading={busy} disabled={!tagAdd.trim()} onClick={applyAddTags}>
            添加
          </Button>
        </div>
      </section>

      <section className="inspector-section">
        <h4>
          <Tags size={12} /> 移除标签
        </h4>
        <div className="inspector-inline-actions">
          <input
            className="input"
            value={tagRemove}
            placeholder="用逗号分隔多个标签"
            disabled={busy}
            onChange={(event) => setTagRemove(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                applyRemoveTags();
              }
            }}
          />
          <Button size="sm" loading={busy} disabled={!tagRemove.trim()} onClick={applyRemoveTags}>
            移除
          </Button>
        </div>
      </section>

      {!inTemp ? (
        <>
          <section className="inspector-section">
            <h4>
              <FolderInput size={12} /> 移动到文件夹
            </h4>
            <div className="inspector-inline-actions">
              <input
                className="input"
                value={folderDraft}
                list="inspector-folders-batch"
                placeholder="目标文件夹路径"
                disabled={busy}
                onChange={(event) => setFolderDraft(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    applyMove();
                  }
                }}
              />
              <Button size="sm" loading={busy} disabled={!folderDraft.trim()} onClick={() => applyMove()}>
                移动
              </Button>
              <IconButton
                label="从文件夹树选择"
                disabled={busy}
                onClick={() => setFolderPickerOpen(true)}
              >
                <FolderInput size={13} />
              </IconButton>
            </div>
            <datalist id="inspector-folders-batch">
              {folders.map((folder) => (
                <option key={folder.id} value={folder.path} />
              ))}
            </datalist>
            <FolderPickerDialog
              open={folderPickerOpen}
              title="移动选中图片"
              subtitle={`将 ${count.toLocaleString()} 张图片移动到…`}
              folders={folders}
              onClose={() => setFolderPickerOpen(false)}
              onSelect={(target) => {
                setFolderPickerOpen(false);
                // 根目录的路径是空串，走专门的“移动到根目录”入口，避免被当成未填写。
                if (target.id == null || target.path === "") moveToRoot();
                else applyMove(target.path);
              }}
            />
            <button
              type="button"
              className="text-button"
              disabled={busy}
              onClick={moveToRoot}
            >
              移动到根目录
            </button>
          </section>

          <section className="inspector-section">
            <h4>
              <Layers size={12} /> 集合
            </h4>
            <div className="inspector-stack">
              <Button size="sm" variant="secondary" icon={<Layers size={13} />} onClick={onAddToCollection}>
                加入集合
              </Button>
              {currentCollectionId != null ? (
                <Button size="sm" variant="ghost" onClick={onRemoveFromCollection}>
                  移出当前集合
                </Button>
              ) : null}
            </div>
          </section>

          <section className="inspector-section inspector-actions">
            <Button size="sm" variant="secondary" icon={<Trash2 size={13} />} onClick={onTrash}>
              移入回收站
            </Button>
            <Button size="sm" variant="primary" icon={<RotateCcw size={13} />} onClick={onRestore}>
              恢复
            </Button>
            <Button size="sm" variant="danger" icon={<Trash2 size={13} />} onClick={onDelete}>
              永久删除
            </Button>
          </section>
        </>
      ) : (
        <section className="inspector-section">
          <span className="field-hint">临时图片关闭、切换图库或退出应用时会自动清空。</span>
        </section>
      )}
    </div>
  );
}
