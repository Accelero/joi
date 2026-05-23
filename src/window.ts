/**
 * Window chrome controls for the custom (decorations:false) titlebar. These are window-management
 * actions, not app/IPC commands — they call Tauri's window API directly. Each is guarded so the
 * browser-only dev preview (no Tauri runtime) silently no-ops instead of throwing.
 */
import { getCurrentWindow } from "@tauri-apps/api/window";

function withWindow(fn: (w: ReturnType<typeof getCurrentWindow>) => Promise<unknown>): void {
  try {
    void fn(getCurrentWindow()).catch(() => {});
  } catch {
    /* not running under Tauri (e.g. plain Vite preview) */
  }
}

export const windowControls = {
  minimize: () => withWindow((w) => w.minimize()),
  toggleMaximize: () => withWindow((w) => w.toggleMaximize()),
  close: () => withWindow((w) => w.close()),
};
