import type { BatchTaskContextInfo } from "../batchTaskContext";
import type { ActionAvailability } from "../capability";
import {
  SCOPE_ORDER,
  scopeCountLabel,
  scopeName,
  type ScopeChoice,
} from "../scope";

interface ScopePickerProps {
  name: string;
  value: ScopeChoice;
  onChange: (choice: ScopeChoice) => void;
  context: BatchTaskContextInfo | null;
  availability: Record<ScopeChoice, ActionAvailability>;
  includeSubfolders?: boolean;
  onIncludeSubfoldersChange?: (value: boolean) => void;
}

export function ScopePicker({
  name,
  value,
  onChange,
  context,
  availability,
  includeSubfolders,
  onIncludeSubfoldersChange,
}: ScopePickerProps) {
  return (
    <>
      <div className="batch-task-scopes" role="radiogroup" aria-label="范围">
        {SCOPE_ORDER.map((choice) => {
          const option = availability[choice];
          return (
            <label
              key={choice}
              className={`batch-task-scope${option.enabled ? "" : " is-disabled"}`}
              title={option.reason}
            >
              <input
                type="radio"
                name={name}
                checked={value === choice}
                disabled={!option.enabled}
                onChange={() => onChange(choice)}
              />
              <span>{scopeName(choice, context)}</span>
              <em>
                {option.enabled
                  ? scopeCountLabel(choice, context, { includeSubfolders })
                  : option.reason}
              </em>
            </label>
          );
        })}
      </div>
      {value === "folder" && onIncludeSubfoldersChange ? (
        <label className="scope-subfolders">
          <input
            type="checkbox"
            checked={includeSubfolders ?? true}
            onChange={(event) => onIncludeSubfoldersChange(event.target.checked)}
          />
          <span>包含子文件夹</span>
        </label>
      ) : null}
    </>
  );
}
