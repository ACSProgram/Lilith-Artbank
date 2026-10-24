import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app/App";
import { initTheme } from "./app/theme";
import "./styles/index.css";

// 先按“跟随系统”上色，设置加载后再按用户选择覆盖，避免首屏闪一下浅色。
initTheme();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
