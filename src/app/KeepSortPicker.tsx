import { ArrowDownUp, Plus, Trash2 } from "lucide-react";
import { Button, IconButton } from "../shared/components/Button";
import { KEEP_SORT_KEYS, SORT_LABELS } from "../modules/library/model";
import type { SortKey, SortSpec } from "../modules/library/types";

interface KeepSortPickerProps {
  sort: SortSpec[];
  onChange: (next: SortSpec[]) => void;
  disabled?: boolean;
}

/** 紧凑的多级排序编辑器：只保留指标与方向，不涉及排序方案。 */
export function KeepSortPicker({ sort, onChange, disabled }: KeepSortPickerProps) {
  const used = new Set(sort.map((spec) => spec.key));
  const canAdd = sort.length < KEEP_SORT_KEYS.length;

  const changeKey = (index: number, key: SortKey) => {
    const next = sort.map((spec) => ({ ...spec }));
    next[index] = { ...next[index], key };
    onChange(next);
  };

  const toggleDirection = (index: number) => {
    const next = sort.map((spec) => ({ ...spec }));
    next[index] = { ...next[index], descending: !next[index].descending };
    onChange(next);
  };

  const addLevel = () => {
    const key = KEEP_SORT_KEYS.find((candidate) => !used.has(candidate));
    if (!key) return;
    onChange([...sort, { key, descending: true }]);
  };

  const removeLevel = (index: number) => {
    onChange(sort.filter((_, position) => position !== index));
  };

  return (
    <div className="keep-sort">
      <span className="keep-sort-label">保留规则</span>
      <div className="keep-sort-levels">
        {sort.map((spec, index) => (
          <div className="keep-sort-level" key={`${spec.key}-${index}`}>
            <span className="keep-sort-index">{index + 1}</span>
            <select
              className="input keep-sort-key"
              value={spec.key}
              disabled={disabled}
              aria-label={`第 ${index + 1} 级保留指标`}
              onChange={(event) => changeKey(index, event.target.value as SortKey)}
            >
              {KEEP_SORT_KEYS.map((key) => (
                <option key={key} value={key} disabled={used.has(key) && key !== spec.key}>
                  {SORT_LABELS[key]}
                </option>
              ))}
            </select>
            <IconButton
              label={spec.descending ? "降序，点击切为升序" : "升序，点击切为降序"}
              disabled={disabled}
              onClick={() => toggleDirection(index)}
            >
              <ArrowDownUp size={13} />
            </IconButton>
            <IconButton
              label="删除这一级"
              disabled={disabled || sort.length <= 1}
              onClick={() => removeLevel(index)}
            >
              <Trash2 size={13} />
            </IconButton>
          </div>
        ))}
        {canAdd ? (
          <Button
            size="sm"
            variant="ghost"
            icon={<Plus size={12} />}
            disabled={disabled}
            onClick={addLevel}
          >
            添加一级
          </Button>
        ) : null}
      </div>
    </div>
  );
}
