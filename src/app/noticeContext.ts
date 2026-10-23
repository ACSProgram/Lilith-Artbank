import { createContext, useContext } from "react";

export type NoticeKind = "info" | "success" | "error";

export interface ConfirmOptions {
  title: string;
  message?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  danger?: boolean;
}

export interface NoticeApi {
  notify: (message: string, kind?: NoticeKind, detail?: string) => void;
  confirm: (options: ConfirmOptions) => Promise<boolean>;
}

export const NoticeContext = createContext<NoticeApi | null>(null);

export function useNotices(): NoticeApi {
  const value = useContext(NoticeContext);
  if (!value) throw new Error("useNotices 必须在 NoticeProvider 内使用");
  return value;
}
