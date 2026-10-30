import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { AlertCircle, CheckCircle2, Info, X } from "lucide-react";
import { ConfirmDialog } from "../shared/components/ConfirmDialog";
import { IconButton } from "../shared/components/Button";
import {
  NoticeContext,
  type ConfirmOptions,
  type NoticeApi,
  type NoticeKind,
} from "../shared/notices";

interface Toast {
  id: number;
  kind: NoticeKind;
  message: string;
  detail?: string;
}

let nextToastId = 1;

function ToastIcon({ kind }: { kind: NoticeKind }) {
  if (kind === "success") return <CheckCircle2 size={18} />;
  if (kind === "error") return <AlertCircle size={18} />;
  return <Info size={18} />;
}

export function NoticeProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [confirmOptions, setConfirmOptions] = useState<ConfirmOptions | null>(null);
  const confirmResolver = useRef<((value: boolean) => void) | null>(null);
  const timers = useRef(new Map<number, number>());

  const dismiss = useCallback((id: number) => {
    const timer = timers.current.get(id);
    if (timer !== undefined) {
      window.clearTimeout(timer);
      timers.current.delete(id);
    }
    setToasts((current) => current.filter((toast) => toast.id !== id));
  }, []);

  const notify = useCallback(
    (message: string, kind: NoticeKind = "info", detail?: string) => {
      const id = nextToastId++;
      setToasts((current) => [...current.slice(-4), { id, kind, message, detail }]);
      const timer = window.setTimeout(() => dismiss(id), kind === "error" ? 7000 : 4200);
      timers.current.set(id, timer);
    },
    [dismiss],
  );

  const confirm = useCallback((options: ConfirmOptions) => {
    return new Promise<boolean>((resolve) => {
      confirmResolver.current?.(false);
      confirmResolver.current = resolve;
      setConfirmOptions(options);
    });
  }, []);

  const resolveConfirm = useCallback((value: boolean) => {
    confirmResolver.current?.(value);
    confirmResolver.current = null;
    setConfirmOptions(null);
  }, []);

  useEffect(() => {
    const pending = timers.current;
    return () => {
      pending.forEach((timer) => window.clearTimeout(timer));
      pending.clear();
    };
  }, []);

  const api = useMemo<NoticeApi>(() => ({ notify, confirm }), [notify, confirm]);

  return (
    <NoticeContext.Provider value={api}>
      {children}
      <div className="toast-viewport" aria-live="polite">
        {toasts.map((toast) => (
          <div key={toast.id} className={`toast toast-${toast.kind}`}>
            <span className="toast-icon">
              <ToastIcon kind={toast.kind} />
            </span>
            <div className="toast-content">
              <span className="toast-message">{toast.message}</span>
              {toast.detail ? <span className="toast-detail">{toast.detail}</span> : null}
            </div>
            <IconButton label="关闭通知" onClick={() => dismiss(toast.id)}>
              <X size={14} />
            </IconButton>
          </div>
        ))}
      </div>
      {confirmOptions ? (
        <ConfirmDialog options={confirmOptions} onResolve={resolveConfirm} />
      ) : null}
    </NoticeContext.Provider>
  );
}
