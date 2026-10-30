import { useEffect, useRef, useState } from "react";
import { FolderInput, Layers, Star, Tags } from "lucide-react";
import { Button, IconButton } from "../../shared/components/Button";
import { FolderPickerDialog } from "../../shared/components/FolderPickerDialog";
import { Spinner } from "../../shared/components/Spinner";
import { TagInput, type TagInputHandle } from "../../shared/components/TagInput";
import { useNotices } from "../../shared/notices";
import { libraryApi } from "./api";
import { useSelectionTags } from "./useSelectionTags";
import type { FolderInfo, SelectionTag } from "./types";
import type { DataChange } from "./dataChange";
import { errorMessage } from "../../shared/errors";
import { SelectionTagChip, TrashActions, searchTagSuggestions } from "./InspectorParts";

export interface BatchInspectorProps {
  imageIds: number[];
  tagVersion: number;
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
}

export function BatchInspector({
  imageIds,
  tagVersion,
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
}: BatchInspectorProps) {
  const { notify } = useNotices();
  const [busy, setBusy] = useState(false);
  const [score, setScore] = useState("unset");
  const [tagAdd, setTagAdd] = useState<string[]>([]);
  const [partialQuery, setPartialQuery] = useState("");
  const tagAddRef = useRef<TagInputHandle>(null);
  const [folderDraft, setFolderDraft] = useState("");
  const [folderPickerOpen, setFolderPickerOpen] = useState(false);

  const count = imageIds.length;
  const {
    tags: selectionTags,
    loading: tagsLoading,
    error: tagsError,
    retry: retryTags,
    forget: forgetSelectionTag,
  } = useSelectionTags(imageIds, tagVersion);

  // 换一批选择时，上一批的暂存标签与搜索词不再适用。
  useEffect(() => {
    setTagAdd([]);
    setPartialQuery("");
  }, [imageIds]);

  const total = selectionTags?.total ?? count;
  const selectionTagList = selectionTags?.tags ?? [];
  const commonTags = selectionTagList.filter((tag) => total > 0 && tag.count >= total);
  const partialTags = selectionTagList.filter((tag) => tag.count < total);
  const needle = partialQuery.trim().toLowerCase();
  const partialMatches = needle
    ? partialTags.filter((tag) => tag.name.toLowerCase().includes(needle))
    : partialTags;
  const visiblePartial = partialMatches.slice(0, 200);
  const commonTagNames = commonTags.map((tag) => tag.name);

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

  const applyAddTags = () => {
    if (busy) return;
    const names = tagAddRef.current?.commitPending() ?? tagAdd;
    if (names.length === 0) return;
    void run(
      async () => {
        const added = await libraryApi.addTagsToImages(imageIds, names);
        setTagAdd([]);
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

  const removeSelectionTag = (tag: SelectionTag) => {
    if (busy) return;
    void run(
      async () => {
        const removed = await libraryApi.removeTagsFromImages(imageIds, [tag.name]);
        forgetSelectionTag(tag.name);
        return `已从 ${removed.toLocaleString()} 张图片移除「${tag.name}」`;
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
        <TagInput
          ref={tagAddRef}
          value={tagAdd}
          onChange={setTagAdd}
          max={64}
          disabled={busy}
          exclude={commonTagNames}
          placeholder="输入标签，回车添加"
          searchSuggestions={searchTagSuggestions}
          commitOnBlur
        />
        <div className="inspector-inline-actions">
          <Button
            size="sm"
            loading={busy}
            disabled={tagAdd.length === 0}
            onClick={applyAddTags}
          >
            添加
          </Button>
          {tagAdd.length > 0 ? (
            <span className="field-hint">将添加 {tagAdd.length} 个标签</span>
          ) : null}
        </div>
      </section>

      <section className="inspector-section">
        <div className="inspector-section-heading">
          <h4>
            <Tags size={12} /> 移除标签
          </h4>
          {tagsLoading ? <Spinner size={12} /> : null}
        </div>
        {tagsError ? (
          <div className="tag-load-error">
            <span>读取选中标签失败：{tagsError}</span>
            <button type="button" className="text-button" onClick={retryTags}>
              重试
            </button>
          </div>
        ) : null}
        {!selectionTags ? (
          tagsError ? null : (
            <div className="inspector-tag-state">
              <Spinner size={12} />
              <span>正在读取标签…</span>
            </div>
          )
        ) : selectionTagList.length === 0 ? (
          <span className="inspector-empty">所选图片还没有标签</span>
        ) : (
          <>
            <div className="tag-group-label">
              全部 {total.toLocaleString()} 张都有
            </div>
            {commonTags.length === 0 ? (
              <span className="inspector-empty">没有共同标签</span>
            ) : (
              <div className="tag-chips">
                {commonTags.map((tag) => (
                  <SelectionTagChip
                    key={tag.name}
                    tag={tag}
                    total={total}
                    busy={busy}
                    onRemove={removeSelectionTag}
                  />
                ))}
              </div>
            )}
            {partialTags.length > 0 ? (
              <details className="selection-tags-partial">
                <summary>部分共同标签 ({partialTags.length})</summary>
                <input
                  className="input"
                  value={partialQuery}
                  placeholder="查找标签"
                  onChange={(event) => setPartialQuery(event.target.value)}
                />
                <div className="tag-chips">
                  {visiblePartial.map((tag) => (
                    <SelectionTagChip
                      key={tag.name}
                      tag={tag}
                      total={total}
                      busy={busy}
                      onRemove={removeSelectionTag}
                    />
                  ))}
                </div>
                {partialMatches.length > visiblePartial.length ? (
                  <span className="field-hint">
                    还有 {partialMatches.length - visiblePartial.length} 个，继续输入以缩小范围。
                  </span>
                ) : null}
              </details>
            ) : null}
          </>
        )}
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
            <TrashActions
              inTrash={inTrash}
              onTrash={onTrash}
              onRestore={onRestore}
              onDelete={onDelete}
            />
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
