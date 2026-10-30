import { createContext, useContext } from "react";

export type NoticeKind = "info" | "success" | "error";

/**
 * 确认框里的可选复选框（例如批量任务的“跳过确认”）。
 * 状态由调用方持有，确认框只负责渲染与回调，返回值仍是 boolean。
 */
export interface ConfirmCheckbox {
  label: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}

export interface ConfirmOptions {
  title: string;
  /** 主说明，直接回答“会发生什么”。 */
  message?: string;
  /** 次要说明，补充后果或不可逆性。 */
  detail?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  danger?: boolean;
  checkbox?: ConfirmCheckbox;
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
