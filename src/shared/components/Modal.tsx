import { useEffect, useRef, type ReactNode } from "react";
import { X } from "lucide-react";

interface ModalProps {
  open: boolean;
  title: string;
  subtitle?: string;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  width?: number;
  dismissible?: boolean;
}

export function Modal({
  open,
  title,
  subtitle,
  onClose,
  children,
  footer,
  width = 560,
  dismissible = true,
}: ModalProps) {
  const panelRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && dismissible) {
        event.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKeyDown, true);
    panelRef.current?.focus();
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [open, dismissible, onClose]);

  if (!open) return null;

  return (
    <div
      className="modal-backdrop"
      onMouseDown={(event) => {
        if (dismissible && event.target === event.currentTarget) onClose();
      }}
    >
      <div
        ref={panelRef}
        className="modal-panel"
        style={{ width }}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        tabIndex={-1}
      >
        <header className="modal-header">
          <div className="modal-heading">
            <h2>{title}</h2>
            {subtitle ? <p>{subtitle}</p> : null}
          </div>
          <button
            type="button"
            className="modal-close"
            onClick={onClose}
            disabled={!dismissible}
            aria-label="关闭"
          >
            <X size={18} />
          </button>
        </header>
        <div className="modal-body">{children}</div>
        {footer ? <footer className="modal-footer">{footer}</footer> : null}
      </div>
    </div>
  );
}
