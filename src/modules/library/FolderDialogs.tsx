import { useEffect, useState } from "react";
import { Button } from "../../shared/components/Button";
import { Field } from "../../shared/components/Field";
import { Modal } from "../../shared/components/Modal";
import {
  FOLDER_SORT_LABELS,
  type FolderSortKey,
} from "./folderTreeModel";

interface FolderNameDialogProps {
  open: boolean;
  title: string;
  subtitle?: string;
  label: string;
  initialValue?: string;
  confirmLabel: string;
  busy?: boolean;
  error?: string | null;
  onClose: () => void;
  onSubmit: (name: string) => void;
}

export function FolderNameDialog({
  open,
  title,
  subtitle,
  label,
  initialValue = "",
  confirmLabel,
  busy = false,
  error,
  onClose,
  onSubmit,
}: FolderNameDialogProps) {
  const [value, setValue] = useState(initialValue);

  useEffect(() => {
    if (open) setValue(initialValue);
  }, [open, initialValue]);

  return (
    <Modal
      open={open}
      title={title}
      subtitle={subtitle}
      onClose={onClose}
      width={440}
      dismissible={!busy}
      footer={
        <>
          <Button variant="ghost" onClick={onClose} disabled={busy}>
            取消
          </Button>
          <Button
            variant="primary"
            loading={busy}
            disabled={!value.trim()}
            onClick={() => onSubmit(value.trim())}
          >
            {confirmLabel}
          </Button>
        </>
      }
    >
      <div className="dialog-form">
        <Field
          label={label}
          hint={'名称不能包含 \\ / : * ? " < > |，也不能以空格或点结尾。'}
        >
          <input
            className="input"
            value={value}
            maxLength={255}
            autoFocus
            spellCheck={false}
            onChange={(event) => setValue(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && value.trim() && !busy) {
                event.preventDefault();
                onSubmit(value.trim());
              }
            }}
          />
        </Field>
        {error ? <div className="form-error">{error}</div> : null}
      </div>
    </Modal>
  );
}

interface FolderSortDialogProps {
  open: boolean;
  path: string;
  canRecurse: boolean;
  busy?: boolean;
  error?: string | null;
  onClose: () => void;
  onSubmit: (key: FolderSortKey, descending: boolean, recursive: boolean) => void;
}

export function FolderSortDialog({
  open,
  path,
  canRecurse,
  busy = false,
  error,
  onClose,
  onSubmit,
}: FolderSortDialogProps) {
  const [key, setKey] = useState<FolderSortKey>("name");
  const [descending, setDescending] = useState(false);
  const [recursive, setRecursive] = useState(false);

  useEffect(() => {
    if (!open) return;
    setKey("name");
    setDescending(false);
    setRecursive(false);
  }, [open]);

  return (
    <Modal
      open={open}
      title="排序子文件夹"
      subtitle={path ? `范围：${path}` : "范围：全部图片的顶层文件夹"}
      onClose={onClose}
      width={460}
      dismissible={!busy}
      footer={
        <>
          <Button variant="ghost" onClick={onClose} disabled={busy}>
            取消
          </Button>
          <Button
            variant="primary"
            loading={busy}
            onClick={() => onSubmit(key, descending, recursive)}
          >
            排序
          </Button>
        </>
      }
    >
      <div className="dialog-form">
        <div className="field">
          <span className="field-label">依据</span>
          <div className="settings-choices">
            {(Object.keys(FOLDER_SORT_LABELS) as FolderSortKey[]).map((value) => (
              <button
                key={value}
                type="button"
                className={`settings-choice${key === value ? " is-active" : ""}`}
                aria-pressed={key === value}
                onClick={() => setKey(value)}
              >
                {FOLDER_SORT_LABELS[value]}
              </button>
            ))}
          </div>
        </div>
        <div className="field">
          <span className="field-label">方向</span>
          <div className="settings-choices">
            <button
              type="button"
              className={`settings-choice${descending ? "" : " is-active"}`}
              aria-pressed={!descending}
              onClick={() => setDescending(false)}
            >
              升序
            </button>
            <button
              type="button"
              className={`settings-choice${descending ? " is-active" : ""}`}
              aria-pressed={descending}
              onClick={() => setDescending(true)}
            >
              降序
            </button>
          </div>
        </div>
        <label className={`folder-sort-recursive${canRecurse ? "" : " is-disabled"}`}>
          <input
            type="checkbox"
            checked={recursive && canRecurse}
            disabled={!canRecurse || busy}
            onChange={(event) => setRecursive(event.target.checked)}
          />
          <span>包含所有后代，每层各自排序</span>
        </label>
        <p className="folder-sort-note">
          会改写这些文件夹的手动顺序，可用一次撤销恢复。
        </p>
        {error ? <div className="form-error">{error}</div> : null}
      </div>
    </Modal>
  );
}
