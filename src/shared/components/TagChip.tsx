import type { MouseEvent, ReactNode } from "react";
import { X } from "lucide-react";

export interface TagChipProps {
  name: string;
  /** 当前标签是否正在参与筛选。 */
  active?: boolean;
  /** 部分图片持有：降低强调，用于批量移除候选。 */
  muted?: boolean;
  /** 名称右侧的附加信息，例如 `全部` 或 `12/30`。 */
  count?: ReactNode;
  /** 名称按钮的 title；不传时使用名称。 */
  nameTitle?: string;
  onNameClick?: (event: MouseEvent<HTMLButtonElement>) => void;
  /** 名称右侧的移除按钮。 */
  onRemove?: () => void;
  removeLabel?: string;
  /** 整块 chip 作为按钮时的动作（批量移除候选）。 */
  onActivate?: () => void;
  activateLabel?: string;
  activateTitle?: string;
  disabled?: boolean;
  /** 整块按钮模式下最右侧的图标，默认 X。 */
  trailing?: ReactNode;
}

/** 标签 chip 的唯一展示基座；不调用后端，不决定筛选或移除语义。 */
export function TagChip({
  name,
  active = false,
  muted = false,
  count,
  nameTitle,
  onNameClick,
  onRemove,
  removeLabel,
  onActivate,
  activateLabel,
  activateTitle,
  disabled = false,
  trailing,
}: TagChipProps) {
  const className = `tag-chip${active ? " is-filtering" : ""}${muted ? " is-partial" : ""}`;

  if (onActivate) {
    return (
      <button
        type="button"
        className={`${className} tag-chip-remove`}
        title={activateTitle ?? name}
        aria-label={activateLabel ?? name}
        disabled={disabled}
        onClick={onActivate}
      >
        <span className="tag-chip-name">{name}</span>
        {count != null ? <em className="tag-chip-count">{count}</em> : null}
        {trailing ?? <X size={10} />}
      </button>
    );
  }

  return (
    <span className={className}>
      {onNameClick ? (
        <button
          type="button"
          className="tag-chip-name"
          title={nameTitle ?? name}
          disabled={disabled}
          onClick={onNameClick}
        >
          {name}
        </button>
      ) : (
        <span className="tag-chip-name" title={name}>
          {name}
        </span>
      )}
      {count != null ? <em className="tag-chip-count">{count}</em> : null}
      {onRemove ? (
        <button
          type="button"
          aria-label={removeLabel ?? `移除标签 ${name}`}
          disabled={disabled}
          onClick={onRemove}
        >
          <X size={10} />
        </button>
      ) : null}
    </span>
  );
}
