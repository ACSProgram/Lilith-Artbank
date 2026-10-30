import type { McpStatus, Settings } from "../types";
import { McpPanel } from "../McpPanel";
import { Panel, type SettingsPatch } from "./SettingsPanel";

export function McpSettingsSection({
  draft,
  patch,
  status,
  statusError,
}: {
  draft: Settings;
  patch: SettingsPatch;
  status: McpStatus | null;
  statusError: string | null;
}) {
  return (
    <Panel title="MCP 服务">
      <McpPanel
        draft={draft}
        patch={patch}
        status={status}
        statusError={statusError}
      />
    </Panel>
  );
}
