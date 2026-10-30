import { RotateCcw, Trash2 } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { TagChip } from "../../shared/components/TagChip";
import { libraryApi } from "./api";
import { imageActions } from "./model";
import type { ColorEntry, SelectionTag } from "./types";

export function searchTagSuggestions(query: string) {
  return libraryApi
    .searchTags(query, 200)
    .then((tags) => tags.map(({ name, imageCount }) => ({ name, imageCount })));
}

export function TrashActions({
  inTrash,
  folderTrashed = false,
  onTrash,
  onRestore,
  onDelete,
}: {
  inTrash: boolean;
  folderTrashed?: boolean;
  onTrash: () => void;
  onRestore: () => void;
  onDelete: () => void;
}) {
  if (inTrash && folderTrashed) {
    return (
      <span className="field-hint">
        图片随文件夹一起移入回收站；请在左侧回收站树里恢复或永久删除整个文件夹。
      </span>
    );
  }
  return imageActions(inTrash).includes("trash") ? (
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
  );
}

export function colorHex(color: ColorEntry): string {
  return `#${[color.r, color.g, color.b]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("")
    .toUpperCase()}`;
}

export function ColorRow({
  label,
  colors,
  onCopy,
}: {
  label: string;
  colors: ColorEntry[];
  onCopy: (text: string, label: string) => void;
}) {
  const visible = colors.filter((color) => color.weight > 0);
  return (
    <div className="color-row">
      <span className="color-row-label">{label}</span>
      {visible.length === 0 ? (
        <span className="inspector-empty">未计算</span>
      ) : (
        <div className="color-swatches">
          {visible.map((color, index) => {
            const percent = Math.round((color.weight / 255) * 100);
            const hex = colorHex(color);
            const title = `${hex} · ${percent}% · rgb(${color.r}, ${color.g}, ${color.b})`;
            return (
              <button
                key={`${hex}-${index}`}
                type="button"
                className="color-swatch"
                title={title}
                aria-label={`${title}，点击复制`}
                style={{
                  background: `rgb(${color.r} ${color.g} ${color.b})`,
                  flexGrow: color.weight,
                }}
                onClick={() => onCopy(hex, "颜色")}
              />
            );
          })}
        </div>
      )}
    </div>
  );
}

export function SelectionTagChip({
  tag,
  total,
  busy,
  onRemove,
}: {
  tag: SelectionTag;
  total: number;
  busy: boolean;
  onRemove: (tag: SelectionTag) => void;
}) {
  const common = total > 0 && tag.count >= total;
  return (
    <TagChip
      name={tag.name}
      muted={!common}
      count={common ? "全部" : `${tag.count}/${total}`}
      activateLabel={
        common
          ? `从全部选中图片移除「${tag.name}」`
          : `从拥有「${tag.name}」的图片移除`
      }
      activateTitle={
        common
          ? `全部 ${total.toLocaleString()} 张都有；点击从全部选中图片移除`
          : `${tag.count.toLocaleString()}/${total.toLocaleString()} 张有；点击从拥有它的图片移除`
      }
      disabled={busy}
      onActivate={() => onRemove(tag)}
    />
  );
}
