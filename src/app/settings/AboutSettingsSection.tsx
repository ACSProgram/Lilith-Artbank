import { ExternalLink, Info } from "lucide-react";
import { Button } from "../../shared/components/Button";
import type { AppInfo } from "../types";
import { Panel } from "./SettingsPanel";

export function AboutSettingsSection({
  appInfo,
  onRevealPath,
}: {
  appInfo: AppInfo | null;
  onRevealPath: (path: string | undefined, failure: string) => void;
}) {
  return (
    <Panel title="关于">
      <div className="settings-field">
        <span className="field-label">设置文件</span>
        <div className="settings-path-row">
          <code className="settings-path">{appInfo?.settingsPath ?? "—"}</code>
          <Button
            size="sm"
            variant="secondary"
            icon={<ExternalLink size={13} />}
            disabled={!appInfo?.settingsPath}
            onClick={() => onRevealPath(appInfo?.settingsPath, "打开设置文件所在文件夹失败")}
          >
            打开文件夹
          </Button>
        </div>
      </div>
      <div className="settings-field">
        <span className="field-label">日志文件</span>
        <div className="settings-path-row">
          <code className="settings-path">{appInfo?.logPath || "—"}</code>
          <Button
            size="sm"
            variant="secondary"
            icon={<ExternalLink size={13} />}
            disabled={!appInfo?.logPath}
            onClick={() => onRevealPath(appInfo?.logPath, "打开日志文件夹失败")}
          >
            打开文件夹
          </Button>
        </div>
      </div>
      <div className="settings-note">
        <Info size={13} />
        删掉设置文件会回到默认值，图库内容不受影响。
      </div>
    </Panel>
  );
}
