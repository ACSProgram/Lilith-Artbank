import type { ConfirmOptions } from "../notices";
import { Button } from "./Button";
import { Modal } from "./Modal";

interface ConfirmDialogProps {
  options: ConfirmOptions;
  onResolve: (value: boolean) => void;
}

export function ConfirmDialog({ options, onResolve }: ConfirmDialogProps) {
  return (
    <Modal
      open
      title={options.title}
      onClose={() => onResolve(false)}
      width={440}
      tone={options.danger ? "danger" : "default"}
      initialFocus="footer"
      footer={
        <>
          <Button variant="ghost" onClick={() => onResolve(false)}>
            {options.cancelLabel ?? "取消"}
          </Button>
          <Button
            variant={options.danger ? "danger" : "primary"}
            onClick={() => onResolve(true)}
          >
            {options.confirmLabel ?? "确定"}
          </Button>
        </>
      }
    >
      {options.message ? <p className="confirm-message">{options.message}</p> : null}
      {options.detail ? <p className="confirm-detail">{options.detail}</p> : null}
      {options.checkbox ? (
        <label className="confirm-checkbox">
          <input
            type="checkbox"
            checked={options.checkbox.checked}
            onChange={(event) => options.checkbox?.onChange(event.target.checked)}
          />
          <span>{options.checkbox.label}</span>
        </label>
      ) : null}
    </Modal>
  );
}
