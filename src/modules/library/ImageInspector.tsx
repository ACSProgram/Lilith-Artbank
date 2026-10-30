import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  Check,
  Copy,
  FileText,
  FolderInput,
  FolderOpen,
  Hash,
  ImageOff,
  Layers,
  Palette,
  Pencil,
  Sparkles,
  Star,
  Tags,
} from "lucide-react";
import { Button, IconButton } from "../../shared/components/Button";
import { FolderPickerDialog } from "../../shared/components/FolderPickerDialog";
import { Popover } from "../../shared/components/Popover";
import { Spinner } from "../../shared/components/Spinner";
import { TagChip } from "../../shared/components/TagChip";
import { TagInput } from "../../shared/components/TagInput";
import { copyText } from "../../shared/clipboard";
import { thumbnailUrl } from "../../shared/thumbUrl";
import { useNotices } from "../../shared/notices";
import { libraryApi } from "./api";
import {
  formatBytes,
  formatDate,
  formatDimensions,
  folderLabel,
} from "./model";
import type { FolderInfo } from "./types";
import type { DataChange } from "./dataChange";
import type { TagFilterAction, TagFilterScope } from "../../shared/tagFilter";
import { errorMessage } from "../../shared/errors";
import { ColorRow, TrashActions, searchTagSuggestions } from "./InspectorParts";
import { useImageDetail } from "./useImageDetail";

function descriptionSourceLabel(source: string): string {
  if (source === "ai") return "AI";
  if (source === "import") return "导入";
  return "手工";
}

export interface ImageInspectorProps {
  imageId: number;
  version: number;
  folders: FolderInfo[];
  currentCollectionId: number | null;
  inTemp: boolean;
  inTrash: boolean;
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
  onOpenFolder?: (folderId: number) => void;
}

export function ImageInspector({
  imageId,
  version,
  folders,
  currentCollectionId,
  inTemp,
  inTrash,
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
  onOpenFolder,
}: ImageInspectorProps) {
  const { notify, confirm } = useNotices();
  const tagMenuAnchorRef = useRef<HTMLDivElement>(null);
  const descriptionViewRef = useRef<HTMLDivElement>(null);
  const descriptionTextareaRef = useRef<HTMLTextAreaElement>(null);
  /** 进入编辑前只读正文的高度；带图片 id，切图后不会套用上一张的高度。 */
  const readonlyHeightRef = useRef<{ id: number; height: number } | null>(null);
  const [tagMenu, setTagMenu] = useState<{
    id: number;
    name: string;
    active: boolean;
  } | null>(null);
  const {
    detail,
    loading,
    loadError,
    folderDraft,
    setFolderDraft,
    thumbFailed,
    setThumbFailed,
    patchDetail,
  } = useImageDetail(imageId, version);
  const [busy, setBusy] = useState(false);
  const [aiBusy, setAiBusy] = useState(false);
  /** 未保存描述按图片 id 暂存；切走再回来仍能恢复编辑态。 */
  const [descriptionDrafts, setDescriptionDrafts] = useState<Map<number, string>>(
    () => new Map(),
  );
  const [folderPickerOpen, setFolderPickerOpen] = useState(false);
  /** 写入返回时对一下当前图：中途切了图就不再改新图的输入框。 */
  const imageIdRef = useRef(imageId);
  imageIdRef.current = imageId;

  useEffect(() => {
    setTagMenu(null);
  }, [imageId]);

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

  const descriptionDraft = detail ? descriptionDrafts.get(detail.id) : undefined;
  const editingDescription = descriptionDraft != null;
  const descriptionDirty =
    detail != null &&
    descriptionDraft != null &&
    descriptionDraft.trim() !== detail.description.trim();
  const descriptionSource =
    detail && detail.description.trim()
      ? descriptionSourceLabel(detail.descriptionSource)
      : null;
  const descriptionHeading = [descriptionSource, descriptionDirty ? "未保存" : null]
    .filter(Boolean)
    .join(" · ");
  const aiBlocked =
    detail != null &&
    detail.descriptionSource === "user" &&
    detail.description.trim() !== "";

  useLayoutEffect(() => {
    if (!editingDescription) return;
    const element = descriptionTextareaRef.current;
    if (!element) return;
    element.style.height = "auto";
    const border = element.offsetHeight - element.clientHeight;
    const measured = readonlyHeightRef.current;
    const readonlyHeight =
      detail != null &&
      measured != null &&
      measured.id === detail.id &&
      descriptionDraft === detail.description
        ? measured.height
        : 0;
    const next = Math.max(element.scrollHeight + border, readonlyHeight);
    element.style.height = `${next}px`;
  }, [editingDescription, detail?.id, descriptionDraft]);

  /** 写入或丢弃（`null`）某张图的描述草稿；保存与取消都走这里。 */
  const setDescriptionDraftFor = (id: number, value: string | null) => {
    setDescriptionDrafts((previous) => {
      if (value == null && !previous.has(id)) return previous;
      const next = new Map(previous);
      if (value == null) next.delete(id);
      else next.set(id, value);
      return next;
    });
  };

  const startEditingDescription = () => {
    if (!detail) return;
    readonlyHeightRef.current = {
      id: detail.id,
      height: descriptionViewRef.current?.offsetHeight ?? 0,
    };
    setDescriptionDraftFor(detail.id, detail.description);
  };

  const cancelDescription = () => {
    if (!detail) return;
    setDescriptionDraftFor(detail.id, null);
  };

  const saveDescription = () => {
    if (!detail || descriptionDraft == null) return;
    const nextDescription = descriptionDraft.trim();
    if (nextDescription === detail.description.trim()) return;
    void run(async () => {
      await libraryApi.updateImageDescription(detail.id, nextDescription);
      patchDetail(detail.id, {
        description: nextDescription,
        descriptionSource: "user",
        descriptionModel: "",
      });
      setDescriptionDraftFor(detail.id, null);
      return {
        kind: "metadata",
        source: "local",
        reason: "description",
        ids: [detail.id],
        fields: ["description"],
        descriptionEmpty: nextDescription === "",
        similarity: "keep",
      };
    }, "描述已保存");
  };

  const generateDescription = async () => {
    if (!onGenerateDescription || !detail || aiBusy || aiBlocked) return;
    if (descriptionDirty) {
      const accepted = await confirm({
        title: "AI 生成会覆盖当前修改？",
        message: "当前描述有未保存的修改，AI 生成会用它返回的文本替换并直接保存。",
        confirmLabel: "生成并保存",
      });
      if (!accepted) return;
    }
    setAiBusy(true);
    try {
      const result = await onGenerateDescription(detail.id);
      patchDetail(detail.id, {
        description: result.text,
        descriptionSource: "ai",
        descriptionModel: result.model,
      });
      setDescriptionDraftFor(detail.id, null);
      notify("AI 描述已生成并保存", "success");
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
      patchDetail(detail.id, { score });
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

  const addTags = (names: string[]) => {
    if (!detail || names.length === 0) return;
    const appliedNames = new Set(detail.tags.map((tag) => tag.name.toLowerCase()));
    const additions = names.filter((name) => {
      const key = name.trim().toLowerCase();
      return key !== "" && !appliedNames.has(key);
    });
    if (additions.length === 0) return;
    void run(async () => {
      const applied = await libraryApi.setImageTags(detail.id, [
        ...detail.tags.map((tag) => tag.name),
        ...additions,
      ]);
      patchDetail(detail.id, { tags: applied });
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
      patchDetail(detail.id, { tags: applied });
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
      if (imageIdRef.current === detail.id) setFolderDraft(target);
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
              <TagChip
                key={tag.id}
                name={tag.name}
                active={active}
                nameTitle={
                  onFilterTag
                    ? active
                      ? "点击可改筛选范围或取消；Ctrl 点击取消该标签"
                      : "点击选择筛选范围；Ctrl 点击直接叠加到当前视图"
                    : undefined
                }
                onNameClick={
                  onFilterTag
                    ? (event) => {
                        if (event.ctrlKey || event.metaKey) {
                          onFilterTag(tag.id, "append", "view");
                          return;
                        }
                        setTagMenu({ id: tag.id, name: tag.name, active });
                      }
                    : undefined
                }
                removeLabel={`移除标签 ${tag.name}`}
                onRemove={() => removeTag(tag.name)}
                disabled={busy}
              />
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
        <TagInput
          value={[]}
          onChange={addTags}
          max={Math.max(0, 64 - detail.tags.length)}
          maxLabel={64}
          disabled={busy}
          exclude={detail.tags.map((tag) => tag.name)}
          placeholder="输入标签，回车添加"
          searchSuggestions={searchTagSuggestions}
        />
      </section>

      <section className="inspector-section">
        <h4>
          <Palette size={12} /> 主色
        </h4>
        <ColorRow label="4 色" colors={detail.colors4} onCopy={copy} />
        <ColorRow label="8 色" colors={detail.colors8} onCopy={copy} />
      </section>

      {!inTemp ? (
        <section className="inspector-section">
          <div className="inspector-section-heading">
            <h4>
              <FolderInput size={12} /> 文件夹
            </h4>
            <span>{folderLabel(detail.folderPath)}</span>
            {onOpenFolder && !inTrash && !detail.folderTrashed ? (
              <IconButton
                label="转到所在文件夹"
                disabled={busy}
                onClick={() => onOpenFolder(detail.folderId)}
              >
                <FolderOpen size={13} />
              </IconButton>
            ) : null}
          </div>
          {detail.folderTrashed ? (
            <span className="field-hint">
              文件夹在回收站中；恢复整个文件夹后才能移动图片。
            </span>
          ) : (
            <>
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
            </>
          )}
        </section>
      ) : null}

      <section className="inspector-section">
        <div className="inspector-section-heading">
          <h4>
            <FileText size={12} /> 描述
          </h4>
          {descriptionHeading ? <span>{descriptionHeading}</span> : null}
        </div>
        {editingDescription ? (
          <>
            <textarea
              ref={descriptionTextareaRef}
              className="input inspector-textarea"
              value={descriptionDraft ?? ""}
              placeholder="记录构图、角色、用途或来源…"
              disabled={busy}
              onChange={(event) => setDescriptionDraftFor(detail.id, event.target.value)}
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
                disabled={!descriptionDirty}
                onClick={saveDescription}
              >
                保存
              </Button>
              {onGenerateDescription ? (
                <Button
                  size="sm"
                  variant="secondary"
                  icon={<Sparkles size={13} />}
                  loading={aiBusy}
                  disabled={busy || detail.deletedAt != null || aiBlocked}
                  title={aiBlocked ? "已有手写描述，AI 不会覆盖" : undefined}
                  onClick={() => void generateDescription()}
                >
                  AI 生成
                </Button>
              ) : null}
              <Button
                size="sm"
                variant="ghost"
                disabled={busy || aiBusy}
                onClick={cancelDescription}
              >
                取消
              </Button>
              <IconButton
                label="复制描述"
                disabled={!(descriptionDraft ?? "").trim()}
                onClick={() => void copy(descriptionDraft ?? "", "描述")}
              >
                <Copy size={13} />
              </IconButton>
            </div>
          </>
        ) : (
          <>
            {detail.description ? (
              <div ref={descriptionViewRef} className="inspector-description">
                {detail.description}
              </div>
            ) : (
              <span className="inspector-empty">还没有描述</span>
            )}
            <div className="inspector-inline-actions">
              <Button
                size="sm"
                variant="secondary"
                icon={<Pencil size={13} />}
                onClick={startEditingDescription}
              >
                编辑描述
              </Button>
              <IconButton
                label="复制描述"
                disabled={!detail.description}
                onClick={() => void copy(detail.description, "描述")}
              >
                <Copy size={13} />
              </IconButton>
            </div>
          </>
        )}
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
          <TrashActions
            inTrash={inTrash}
            folderTrashed={detail.folderTrashed}
            onTrash={onTrash}
            onRestore={onRestore}
            onDelete={onDelete}
          />
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
