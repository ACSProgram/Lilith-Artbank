/**
 * Tauri 命令失败时返回字符串，浏览器里抛出的则是 Error；统一成可直接展示的文案。
 */
export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "未知错误";
}
