import type { ConfirmOptions } from "../../app/noticeContext";
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
      footer={
        <>
          <Button variant="ghost" onClick={() => onResolve(false)} autoFocus>
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
      <p className="confirm-message">{options.message}</p>
    </Modal>
  );
}
