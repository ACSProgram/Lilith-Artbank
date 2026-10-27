import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
} from "react";
import { Plus, X } from "lucide-react";
import { IconButton } from "./Button";
import { mergeTagNames, normalizeTagName } from "../tagNames";

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
  searchSuggestions?: (query: string) => Promise<string[]>;
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
  const [suggestions, setSuggestions] = useState<string[]>([]);
  const inputRef = useRef<HTMLInputElement>(null);
  const remaining = max == null ? Number.POSITIVE_INFINITY : Math.max(0, max - value.length);
  const inputDisabled = disabled || remaining <= 0;

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
    return next;
  }, [commitName, text]);

  useImperativeHandle(ref, () => ({ commitPending: commitText }), [commitText]);

  // 调用方每次渲染都可能新建数组；用拼接后的键做依赖，内容不变就不重查建议。
  const valueKey = value.join("\u0000");
  const excludeKey = (exclude ?? []).join("\u0000");
  useEffect(() => {
    const needle = text.trim();
    if (!needle || !searchSuggestions) {
      setSuggestions([]);
      return;
    }
    let disposed = false;
    const timer = window.setTimeout(() => {
      void searchSuggestions(needle)
        .then((names) => {
          if (disposed) return;
          const blocked = new Set(
            [...(exclude ?? []), ...value].map((name) => name.trim().toLowerCase()),
          );
          setSuggestions(
            names
              .map((name) => name.trim())
              .filter((name) => name && !blocked.has(name.toLowerCase()))
              .slice(0, 12),
          );
        })
        .catch(() => {
          if (!disposed) setSuggestions([]);
        });
    }, 160);
    return () => {
      disposed = true;
      window.clearTimeout(timer);
    };
  }, [text, searchSuggestions, valueKey, excludeKey]);

  return (
    <div className="tag-editor">
      {value.length > 0 ? (
        <div className="tag-chips tag-editor-chips">
          {value.map((name, index) => (
            <span key={`${name}-${index}`} className="tag-chip">
              <span className="tag-chip-name">{name}</span>
              <button
                type="button"
                aria-label={`移除标签 ${name}`}
                disabled={disabled}
                onClick={() => onChange(value.filter((_, item) => item !== index))}
              >
                <X size={10} />
              </button>
            </span>
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
          onChange={(event) => setText(event.target.value)}
          onKeyDown={(event) => {
            if (event.nativeEvent.isComposing) return;
            if (event.key === "Enter") {
              event.preventDefault();
              commitText();
            } else if (event.key === "Backspace" && text === "" && value.length > 0) {
              event.preventDefault();
              onChange(value.slice(0, -1));
            }
          }}
          onBlur={() => {
            if (commitOnBlur) commitText();
          }}
        />
        <IconButton
          label="添加标签"
          disabled={inputDisabled || !text.trim()}
          onClick={() => {
            commitText();
            inputRef.current?.focus();
          }}
        >
          <Plus size={13} />
        </IconButton>
        {suggestions.length > 0 ? (
          <div className="tag-suggestions">
            {suggestions.map((name) => (
              <button
                key={name}
                type="button"
                onMouseDown={(event) => event.preventDefault()}
                onClick={() => {
                  commitName(name);
                  setText("");
                  setSuggestions([]);
                  inputRef.current?.focus();
                }}
              >
                {name}
              </button>
            ))}
          </div>
        ) : null}
      </div>
      {max != null && remaining <= 0 ? (
        <span className="field-hint">已达到 {maxLabel ?? max} 个标签上限。</span>
      ) : null}
    </div>
  );
});
