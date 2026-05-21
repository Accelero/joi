import { useState } from "react";
import type { AppState } from "./ipc";

/**
 * Placeholder app chrome. The real terminal (xterm.js), controls, and media wiring land with the
 * Tauri shell (PLAN M1+); this renders the lifecycle state so the bundle has an entry point and the
 * IPC types are exercised. Media never flows through React state (SPEC §8.2).
 */
export function App(): React.JSX.Element {
  const [state] = useState<AppState>("stopped");

  return (
    <main className="p-6">
      <h1 className="text-xl font-semibold text-slate-200">Joi</h1>
      <p className="mt-2 text-slate-400">
        Local voice companion — state: <span className="text-emerald-400">{state}</span>
      </p>
    </main>
  );
}
