import { useEffect, useState } from "react";
import { Button } from "../../shared/components/Button";
import { Field } from "../../shared/components/Field";
import { Modal } from "../../shared/components/Modal";

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
