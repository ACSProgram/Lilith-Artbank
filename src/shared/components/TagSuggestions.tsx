import type { ReactNode } from "react";
import type { TagSuggestion } from "../tagNames";

export interface TagSuggestionsProps {
  items: TagSuggestion[];
  onPick: (name: string) => void;
  /** 内联展开：用于 modal 里的改名/合并，不用绝对定位。 */
  inline?: boolean;
  /** 底部说明，例如“回车添加「…」”。 */
  footnote?: ReactNode;
  /** 没有任何建议时仍可显示的一句提示。 */
  emptyText?: string;
}

/** 标签建议列表的唯一展示组件；不发起查询，不决定是否打开。 */
export function TagSuggestions({
  items,
  onPick,
  inline = false,
  footnote,
  emptyText,
}: TagSuggestionsProps) {
  if (items.length === 0 && !footnote && !emptyText) return null;
  return (
    <div className={`tag-suggestions${inline ? " is-inline" : ""}`}>
      {items.map((item) => (
        <button
          key={item.name}
          type="button"
          onMouseDown={(event) => event.preventDefault()}
          onClick={() => onPick(item.name)}
        >
          <span className="tag-suggestions-name" title={item.name}>
            {item.name}
          </span>
          {item.imageCount != null ? (
            <span className="tag-suggestions-count">
              {item.imageCount.toLocaleString()} 张
            </span>
          ) : null}
        </button>
      ))}
      {items.length === 0 && emptyText ? (
        <p className="tag-suggestions-empty">{emptyText}</p>
      ) : null}
      {footnote ? <div className="tag-suggestions-footnote">{footnote}</div> : null}
    </div>
  );
}
