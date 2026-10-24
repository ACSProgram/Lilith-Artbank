import { AppRuntimeProvider } from "./AppRuntimeProvider";
import { AppShell } from "./AppShell";
import { NoticeProvider } from "./NoticeProvider";
import { TaskCenterProvider } from "./TaskCenterProvider";

export function App() {
  return (
    <NoticeProvider>
      <AppRuntimeProvider>
        <TaskCenterProvider>
          <AppShell />
        </TaskCenterProvider>
      </AppRuntimeProvider>
    </NoticeProvider>
  );
}
