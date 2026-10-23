import { useEffect, useState } from "react";
import { Copy, Minus, Square, X } from "lucide-react";
import { IconButton } from "../shared/components/Button";
import { appApi, browserPreview } from "./api";
import { useAppRuntime } from "./runtimeContext";

export function WindowTitleBar() {
  const { current, busy } = useAppRuntime();
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    void appApi
      .windowIsMaximized()
      .then(setMaximized)
      .catch(() => undefined);
  }, []);

  const toggleMaximize = async () => {
    try {
      setMaximized(await appApi.windowToggleMaximize());
    } catch {
      // 窗口控制失败不阻断业务操作。
    }
  };

  return (
    <header className="titlebar">
      <div className="titlebar-brand" data-tauri-drag-region onDoubleClick={toggleMaximize}>
        <span className="brand-mark">A</span>
        <span className="brand-name">Lilith Artbank</span>
        {browserPreview ? <span className="titlebar-badge">浏览器预览</span> : null}
        {current ? (
          <>
            <span className="titlebar-separator">/</span>
            <span className="titlebar-library" title={current.path}>
              {current.name}
            </span>
          </>
        ) : null}
        {busy ? <span className="titlebar-busy">{busy}…</span> : null}
      </div>
      <div className="titlebar-drag" data-tauri-drag-region onDoubleClick={toggleMaximize} />
      <div className="titlebar-actions">
        <IconButton label="最小化" onClick={() => void appApi.windowMinimize()}>
          <Minus size={15} />
        </IconButton>
        <IconButton
          label={maximized ? "还原" : "最大化"}
          onClick={() => void toggleMaximize()}
        >
          {maximized ? <Copy size={13} /> : <Square size={13} />}
        </IconButton>
        <IconButton
          label="关闭"
          variant="danger"
          onClick={() => void appApi.windowClose()}
        >
          <X size={16} />
        </IconButton>
      </div>
    </header>
  );
}
