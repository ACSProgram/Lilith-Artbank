import type { ThemeMode } from "./types";

type ResolvedTheme = "light" | "dark";

const DARK_QUERY = "(prefers-color-scheme: dark)";

let mode: ThemeMode = "system";
let initialized = false;

function darkQuery(): MediaQueryList | null {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return null;
  return window.matchMedia(DARK_QUERY);
}

/** `system` 交给操作系统，其余原样返回。 */
function resolveTheme(next: ThemeMode): ResolvedTheme {
  if (next === "light" || next === "dark") return next;
  return darkQuery()?.matches ? "dark" : "light";
}

function paint() {
  if (typeof document === "undefined") return;
  document.documentElement.dataset.theme = resolveTheme(mode);
}

/** 写入当前主题并立即应用；`system` 下系统配色变化时自动跟随。 */
export function setThemeMode(next: ThemeMode) {
  // 重复写入同一个值不必再动 DOM（设置页取消预览时会再设一次已保存的值）。
  if (initialized && next === mode) return;
  mode = next;
  paint();
}

/** 在首次渲染前调用，避免先用浅色闪一下再切到深色。 */
export function initTheme() {
  if (initialized) return;
  initialized = true;
  darkQuery()?.addEventListener("change", () => {
    if (mode === "system") paint();
  });
  paint();
}
