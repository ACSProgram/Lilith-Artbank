import { useEffect, useRef, type ReactNode } from "react";
import { X } from "lucide-react";

const FOCUSABLE = [
  "a[href]",
  "button:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  '[tabindex]:not([tabindex="-1"])',
].join(",");

/**
 * 叠层弹窗栈。嵌套弹窗各自独立挂载、没有共同的 React 祖先，
 * 所以“谁是当前最上层”只能放在模块级：只有栈顶响应 Esc 与 Tab，
 * 滚动锁也跟随栈空/非空，内层关闭不会提前解锁。
 */
const modalStack: HTMLElement[] = [];

function pushModal(panel: HTMLElement) {
  if (modalStack.length === 0) document.body.dataset.scrollLock = "true";
  modalStack.push(panel);
}

function popModal(panel: HTMLElement) {
  const index = modalStack.lastIndexOf(panel);
  if (index !== -1) modalStack.splice(index, 1);
  if (modalStack.length === 0) delete document.body.dataset.scrollLock;
}

function isTopModal(panel: HTMLElement): boolean {
  return modalStack[modalStack.length - 1] === panel;
}

/** 只算真正可见的可聚焦元素；`offsetParent` 为空的（折叠区、`display:none`）跳过。 */
function focusableIn(root: HTMLElement | null): HTMLElement[] {
  if (!root) return [];
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (element) => element.offsetParent !== null,
  );
}

interface ModalProps {
  open: boolean;
  title: string;
  subtitle?: string;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  width?: number;
  dismissible?: boolean;
  tone?: "default" | "danger";
  /**
   * 打开时的焦点落点：`panel`（默认）交给对话框本身，读屏从标题开始；
   * `footer` 交给页脚第一个按钮——确认框要停在“取消”上，而不是标题栏的关闭按钮。
   */
  initialFocus?: "panel" | "footer";
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
  tone = "default",
  initialFocus = "panel",
}: ModalProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  // onClose 常是内联箭头函数：放进 ref，避免每次重渲染都重新跑副作用、把焦点抢回面板。
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    if (!open) return;
    const panel = panelRef.current;
    if (!panel) return;
    pushModal(panel);

    const onKeyDown = (event: KeyboardEvent) => {
      // 叠层时下层弹窗不参与按键处理，否则 Esc 会关掉被遮住的那一层。
      if (!isTopModal(panel)) return;
      if (event.key === "Escape") {
        if (!dismissible) return;
        event.stopPropagation();
        onCloseRef.current();
        return;
      }
      if (event.key !== "Tab") return;
      // 焦点陷阱：Tab / Shift+Tab 只在面板内部循环，不会跑到背景上。
      const items = focusableIn(panel);
      if (items.length === 0) {
        event.preventDefault();
        panel.focus();
        return;
      }
      const first = items[0];
      const last = items[items.length - 1];
      const active = document.activeElement as HTMLElement | null;
      const inside = panel.contains(active);
      if (event.shiftKey) {
        if (!inside || active === first) {
          event.preventDefault();
          last.focus();
        }
      } else if (!inside || active === last) {
        event.preventDefault();
        first.focus();
      }
    };

    window.addEventListener("keydown", onKeyDown, true);
    const target =
      initialFocus === "footer"
        ? (focusableIn(panel.querySelector<HTMLElement>(".modal-footer"))[0] ?? panel)
        : panel;
    target.focus();
    return () => {
      window.removeEventListener("keydown", onKeyDown, true);
      popModal(panel);
    };
  }, [open, dismissible, initialFocus]);

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
        className={`modal-panel${tone === "danger" ? " modal-panel-danger" : ""}`}
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
