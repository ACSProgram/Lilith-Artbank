import { Database, ExternalLink, FolderOpen, LogOut } from "lucide-react";
import { Button } from "../../shared/components/Button";
import type { LibraryInfo } from "../types";
import { Panel } from "./SettingsPanel";

export function LibrarySettingsSection({
  current,
  onChangeLibrary,
  onRevealPath,
  onCloseLibrary,
}: {
  current: LibraryInfo | null;
  onChangeLibrary: () => void;
  onRevealPath: (path: string | undefined, failure: string) => void;
  onCloseLibrary: () => void;
}) {
  return (
    <Panel title="当前图库">
      {current ? (
        <div className="settings-library-card">
          <Database size={16} />
          <div className="settings-library-info">
            <strong>{current.name}</strong>
            <span title={current.path}>{current.path}</span>
          </div>
          <span className="settings-library-count">
            {current.imageCount.toLocaleString()} 张
          </span>
        </div>
      ) : (
        <div className="settings-library-card">
          <Database size={16} />
          <div className="settings-library-info">
            <strong>尚未打开图库</strong>
          </div>
        </div>
      )}
      <div className="settings-inline-actions">
        <Button size="sm" variant="secondary" icon={<FolderOpen size={14} />} onClick={onChangeLibrary}>
          {current ? "更换图库" : "打开图库"}
        </Button>
        {current ? (
          <Button
            size="sm"
            variant="ghost"
            icon={<ExternalLink size={14} />}
            onClick={() => onRevealPath(current.path, "打开图库目录失败")}
          >
            打开图库目录
          </Button>
        ) : null}
        {current ? (
          <Button
            size="sm"
            variant="danger"
            icon={<LogOut size={14} />}
            onClick={onCloseLibrary}
          >
            关闭图库
          </Button>
        ) : null}
      </div>
      <div className="settings-note">关闭不会删除任何图片或标签、评分等信息。</div>
    </Panel>
  );
}
