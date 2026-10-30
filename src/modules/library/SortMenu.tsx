import { useEffect, useRef, useState } from "react";
import { ArrowDownUp, ChevronDown, ChevronUp, Plus, Trash2 } from "lucide-react";
import type { SortProfile } from "../../shared/settings";
import { Button, IconButton } from "../../shared/components/Button";
import { Popover } from "../../shared/components/Popover";
import {
  SORT_KEYS,
  SORT_LABELS,
  sortKeyAvailable,
  sortSummary,
} from "./model";
import type { LibraryView, SortKey, SortSpec } from "./types";

interface SortMenuProps {
  sort: SortSpec[];
  profiles: SortProfile[];
  view: LibraryView;
  disabled?: boolean;
  onSortChange: (sort: SortSpec[]) => void;
  onProfilesChange: (profiles: SortProfile[]) => void;
}

/** 与 Rust `settings::MAX_SORT_PROFILES` 保持一致：方案数量是存储上限，与排序级数无关。 */
const MAX_PROFILES = 50;
const TEMPORARY = "__temporary__";

function cloneSort(sort: SortSpec[]): SortSpec[] {
  return sort.map((spec) => ({ ...spec }));
}

export function SortMenu({
  sort,
  profiles,
  view,
  disabled,
  onSortChange,
  onProfilesChange,
}: SortMenuProps) {
  const [open, setOpen] = useState(false);
  const [selectedProfile, setSelectedProfile] = useState(TEMPORARY);
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const anchorRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    setError(null);
    const current = profiles.find((profile) => profile.id === selectedProfile);
    if (current) setName(current.name);
  }, [open, profiles, selectedProfile]);

  const used = new Set(sort.map((spec) => spec.key));
  const canAdd = SORT_KEYS.some((key) => !used.has(key) && sortKeyAvailable(key, view));
  const activeProfile = profiles.find((profile) => profile.id === selectedProfile) ?? null;

  const update = (next: SortSpec[]) => {
    onSortChange(next);
    setError(null);
  };

  const changeKey = (index: number, key: SortKey) => {
    const next = cloneSort(sort);
    next[index] = { ...next[index], key };
    update(next);
  };

  const addLevel = () => {
    const key = SORT_KEYS.find((candidate) => !used.has(candidate) && sortKeyAvailable(candidate, view));
    if (!key) return;
    update([...sort, { key, descending: key !== "collectionPosition" }]);
  };

  const moveLevel = (index: number, delta: number) => {
    const target = index + delta;
    if (target < 0 || target >= sort.length) return;
    const next = cloneSort(sort);
    [next[index], next[target]] = [next[target], next[index]];
    update(next);
  };

  const removeLevel = (index: number) => {
    if (sort.length <= 1) return;
    update(sort.filter((_, position) => position !== index));
  };

  const chooseProfile = (value: string) => {
    setSelectedProfile(value);
    setError(null);
    const profile = profiles.find((item) => item.id === value);
    if (!profile) {
      setName("");
      return;
    }
    setName(profile.name);
    const available = profile.sort.filter((spec) => sortKeyAvailable(spec.key, view));
    update(
      available.length > 0
        ? cloneSort(available)
        : [{ key: "importedAt", descending: true }],
    );
  };

  const saveProfile = () => {
    const trimmed = name.trim();
    if (!trimmed) {
      setError("先给方案起个名字");
      return;
    }
    if (profiles.some((profile) => profile.name.toLowerCase() === trimmed.toLowerCase())) {
      setError("已经有同名方案");
      return;
    }
    if (profiles.length >= MAX_PROFILES) {
      setError(`最多保存 ${MAX_PROFILES} 个方案`);
      return;
    }
    const profile: SortProfile = {
      id: crypto.randomUUID(),
      name: trimmed,
      sort: cloneSort(sort),
    };
    onProfilesChange([...profiles, profile]);
    setSelectedProfile(profile.id);
    setError(null);
  };

  const updateProfile = () => {
    if (!activeProfile) {
      setError("先选择一个已保存的方案");
      return;
    }
    const trimmed = name.trim();
    if (!trimmed) {
      setError("方案名称不能为空");
      return;
    }
    if (
      profiles.some(
        (profile) =>
          profile.id !== activeProfile.id &&
          profile.name.toLowerCase() === trimmed.toLowerCase(),
      )
    ) {
      setError("已经有同名方案");
      return;
    }
    onProfilesChange(
      profiles.map((profile) =>
        profile.id === activeProfile.id
          ? { ...profile, name: trimmed, sort: cloneSort(sort) }
          : profile,
      ),
    );
    setError(null);
  };

  const deleteProfile = () => {
    if (!activeProfile) return;
    onProfilesChange(profiles.filter((profile) => profile.id !== activeProfile.id));
    setSelectedProfile(TEMPORARY);
    setName("");
    setError(null);
  };

  return (
    <>
      <button
        ref={anchorRef}
        type="button"
        className="button button-secondary button-sm sort-button"
        aria-expanded={open}
        disabled={disabled}
        title={`排序：${sortSummary(sort)}`}
        onClick={() => setOpen((value) => !value)}
      >
        <ArrowDownUp size={14} />
        <span className="sort-button-label">{sortSummary(sort)}</span>
      </button>
      <Popover
        open={open}
        anchorRef={anchorRef}
        placement="bottom-end"
        onClose={() => setOpen(false)}
        className="sort-menu"
        minWidth={330}
      >
        <div className="sort-menu-levels">
          {sort.map((spec, index) => (
            <div className="sort-level" key={`${spec.key}-${index}`}>
              <span className="sort-level-index">{index + 1}</span>
              <select
                className="input sort-level-key"
                value={spec.key}
                aria-label={`第 ${index + 1} 级排序指标`}
                onChange={(event) => changeKey(index, event.target.value as SortKey)}
              >
                {SORT_KEYS.map((key) => {
                  const available = sortKeyAvailable(key, view);
                  return (
                    <option
                      key={key}
                      value={key}
                      disabled={(used.has(key) && key !== spec.key) || !available}
                    >
                      {SORT_LABELS[key]}
                      {available ? "" : "（仅集合视图）"}
                    </option>
                  );
                })}
              </select>
              <IconButton
                label={spec.descending ? "降序，点击切为升序" : "升序，点击切为降序"}
                onClick={() => {
                  const next = cloneSort(sort);
                  next[index] = { ...spec, descending: !spec.descending };
                  update(next);
                }}
              >
                <ArrowDownUp size={13} />
              </IconButton>
              <IconButton
                label="上移"
                disabled={index === 0}
                onClick={() => moveLevel(index, -1)}
              >
                <ChevronUp size={13} />
              </IconButton>
              <IconButton
                label="下移"
                disabled={index === sort.length - 1}
                onClick={() => moveLevel(index, 1)}
              >
                <ChevronDown size={13} />
              </IconButton>
              <IconButton
                label="删除这一级"
                disabled={sort.length <= 1}
                onClick={() => removeLevel(index)}
              >
                <Trash2 size={13} />
              </IconButton>
            </div>
          ))}
        </div>

        <div className="sort-menu-footer">
          <Button
            size="sm"
            variant="secondary"
            icon={<Plus size={13} />}
            disabled={!canAdd}
            title={canAdd ? undefined : "没有还能用于当前视图的指标"}
            onClick={addLevel}
          >
            添加一级
          </Button>
          {sort.length > 3 ? (
            <span className="sort-hint">级数较多时，第一次载入会稍慢。</span>
          ) : null}
        </div>

        <div className="sort-menu-profiles">
          <span className="menu-section-title">已保存方案</span>
          <div className="sort-profile-row">
            <select
              className="input sort-profile-select"
              value={selectedProfile}
              aria-label="已保存的排序方案"
              onChange={(event) => chooseProfile(event.target.value)}
            >
              <option value={TEMPORARY}>临时排序</option>
              {profiles.map((profile) => (
                <option key={profile.id} value={profile.id}>
                  {profile.name}
                </option>
              ))}
            </select>
            <input
              className="input sort-profile-name"
              value={name}
              placeholder="方案名称"
              aria-label="方案名称"
              onChange={(event) => setName(event.target.value)}
            />
          </div>
          <div className="sort-profile-actions">
            <Button size="sm" variant="secondary" onClick={saveProfile}>
              另存为新方案
            </Button>
            <Button
              size="sm"
              variant="secondary"
              disabled={!activeProfile}
              onClick={updateProfile}
            >
              更新方案
            </Button>
            <Button
              size="sm"
              variant="ghost"
              disabled={!activeProfile}
              onClick={deleteProfile}
            >
              删除方案
            </Button>
          </div>
          {error ? <span className="sort-error">{error}</span> : null}
        </div>
      </Popover>
    </>
  );
}
