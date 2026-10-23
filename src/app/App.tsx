import { AppRuntimeProvider } from "./AppRuntimeProvider";
import { AppShell } from "./AppShell";
import { NoticeProvider } from "./NoticeProvider";

export function App() {
  return (
    <NoticeProvider>
      <AppRuntimeProvider>
        <AppShell />
      </AppRuntimeProvider>
    </NoticeProvider>
  );
}
