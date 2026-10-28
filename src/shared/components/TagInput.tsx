import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
} from "react";
import { Plus } from "lucide-react";
import { IconButton } from "./Button";
import { TagChip } from "./TagChip";
import { TagSuggestions } from "./TagSuggestions";
import {
  filterTagSuggestions,
  mergeTagNames,
  normalizeTagName,
  tagSuggestionRequest,
  type TagSuggestion,
} from "../tagNames";

export interface TagInputHandle {
  /** 提交输入框里尚未回车的文本，返回提交后的完整 chip 列表。 */
  commitPending: () => string[];
}

export interface TagInputProps {
  value: string[];
  onChange: (next: string[]) => void;
  max?: number;
  /** 上限提示里显示的总数；详情把自己已有的标签算进去时用它保持 64。 */
  maxLabel?: number;
  disabled?: boolean;
  placeholder?: string;
  exclude?: string[];
  searchSuggestions?: (query: string) => Promise<TagSuggestion[]>;
  /** 草稿模式开启：点外部按钮前先提交未回车的文本，避免丢字。 */
  commitOnBlur?: boolean;
}

/**
 * chip 式标签输入：一次提交一个标签，不按逗号/分号/空格拆分。
 * 只负责输入与展示，不调用后端；自动补全由使用方注入。
 */
export const TagInput = forwardRef<TagInputHandle, TagInputProps>(function TagInput(
  {
    value,
    onChange,
    max,
    maxLabel,
    disabled = false,
    placeholder = "输入标签，回车添加",
    exclude,
    searchSuggestions,
    commitOnBlur = false,
  },
  ref,
) {
  const [text, setText] = useState("");
  const [suggestions, setSuggestions] = useState<TagSuggestion[]>([]);
  const [open, setOpen] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const remaining = max == null ? Number.POSITIVE_INFINITY : Math.max(0, max - value.length);
  const inputDisabled = disabled || remaining <= 0;
  const needle = text.trim();

  const commitName = useCallback(
    (raw: string): string[] => {
      const name = normalizeTagName(raw);
      if (!name || disabled || remaining <= 0) return value;
      const next = mergeTagNames(value, [name], { max, exclude });
      if (next.length !== value.length) onChange(next);
      return next;
    },
    [value, disabled, remaining, max, exclude, onChange],
  );

  const commitText = useCallback((): string[] => {
    const next = commitName(text);
    setText("");
    setSuggestions([]);
    setOpen(false);
    return next;
  }, [commitName, text]);

  useImperativeHandle(ref, () => ({ commitPending: commitText }), [commitText]);

  // 调用方每次渲染都可能新建数组；用拼接后的键做依赖，内容不变就不重查建议。
  const valueKey = value.join("\u0000");
  const excludeKey = (exclude ?? []).join("\u0000");
  useEffect(() => {
    if (!open || inputDisabled || !searchSuggestions) {
      setSuggestions([]);
      return;
    }
    const request = tagSuggestionRequest(text);
    let disposed = false;
    const timer = window.setTimeout(() => {
      void searchSuggestions(request.needle)
        .then((items) => {
          if (disposed) return;
          setSuggestions(filterTagSuggestions(items, [...(exclude ?? []), ...value]));
        })
        .catch(() => {
          if (disposed) return;
          setSuggestions([]);
          setOpen(false);
        });
    }, request.debounceMs);
    return () => {
      disposed = true;
      window.clearTimeout(timer);
    };
  }, [open, text, searchSuggestions, valueKey, excludeKey, inputDisabled]);

  const pickSuggestion = (name: string) => {
    commitName(name);
    setText("");
    setSuggestions([]);
    setOpen(false);
    inputRef.current?.focus();
  };

  return (
    <div className="tag-editor">
      {value.length > 0 ? (
        <div className="tag-chips tag-editor-chips">
          {value.map((name, index) => (
            <TagChip
              key={`${name}-${index}`}
              name={name}
              disabled={disabled}
              removeLabel={`移除标签 ${name}`}
              onRemove={() => onChange(value.filter((_, item) => item !== index))}
            />
          ))}
        </div>
      ) : null}
      <div className="tag-input-wrap">
        <input
          ref={inputRef}
          className="input"
          value={text}
          placeholder={remaining <= 0 ? "已达到标签上限" : placeholder}
          disabled={inputDisabled}
          autoComplete="off"
          spellCheck={false}
          onChange={(event) => {
            setText(event.target.value);
            setOpen(true);
          }}
          onFocus={() => setOpen(true)}
          onKeyDown={(event) => {
            if (event.nativeEvent.isComposing) return;
            if (event.key === "Enter") {
              event.preventDefault();
              commitText();
            } else if (event.key === "Escape" && open) {
              event.preventDefault();
              event.stopPropagation();
              setOpen(false);
            } else if (event.key === "Backspace" && text === "" && value.length > 0) {
              event.preventDefault();
              onChange(value.slice(0, -1));
            }
          }}
          onBlur={() => {
            if (commitOnBlur) {
              commitText();
              return;
            }
            setOpen(false);
          }}
        />
        <IconButton
          label="添加标签"
          disabled={inputDisabled || !needle}
          onClick={() => {
            commitText();
            inputRef.current?.focus();
          }}
        >
          <Plus size={13} />
        </IconButton>
        {open && !inputDisabled && searchSuggestions && (suggestions.length > 0 || needle) ? (
          <TagSuggestions
            items={suggestions}
            onPick={pickSuggestion}
            emptyText={needle ? "没有匹配的标签" : undefined}
            footnote={needle ? <>回车添加「{needle}」</> : undefined}
          />
        ) : null}
      </div>
      {max != null && remaining <= 0 ? (
        <span className="field-hint">已达到 {maxLabel ?? max} 个标签上限。</span>
      ) : null}
    </div>
  );
});
