/**
 * Session controls + the always-visible lifecycle state indicator (FR-4). Buttons are disabled
 * while a start/stop transition is in flight so the user can't double-fire.
 */
import type { AppState, ConnectionStatus } from "../ipc";

interface ControlsProps {
  state: AppState;
  connection: ConnectionStatus;
  micMuted: boolean;
  busy: boolean;
  onStart(): void;
  onStop(): void;
  onToggleMute(): void;
}

const STATE_COLOR: Record<AppState, string> = {
  stopped: "bg-slate-500",
  connecting: "bg-amber-400",
  listening: "bg-emerald-400",
  thinking: "bg-sky-400",
  speaking: "bg-violet-400",
  reconnecting: "bg-amber-400",
  error: "bg-rose-500",
};

export function Controls({
  state,
  connection,
  micMuted,
  busy,
  onStart,
  onStop,
  onToggleMute,
}: ControlsProps): React.JSX.Element {
  const running = state !== "stopped" && state !== "error";

  return (
    <div className="flex items-center gap-3 border-b border-slate-800 px-4 py-3">
      <span className="flex items-center gap-2 text-sm text-slate-300">
        <span className={`inline-block h-2.5 w-2.5 rounded-full ${STATE_COLOR[state]}`} />
        {state}
      </span>

      {running ? (
        <button
          type="button"
          onClick={onStop}
          disabled={busy}
          className="rounded bg-rose-600 px-3 py-1 text-sm text-white disabled:opacity-50"
        >
          Stop
        </button>
      ) : (
        <button
          type="button"
          onClick={onStart}
          disabled={busy}
          className="rounded bg-emerald-600 px-3 py-1 text-sm text-white disabled:opacity-50"
        >
          Start
        </button>
      )}

      <button
        type="button"
        onClick={onToggleMute}
        disabled={!running}
        className="rounded bg-slate-700 px-3 py-1 text-sm text-white disabled:opacity-50"
      >
        {micMuted ? "Unmute" : "Mute"}
      </button>

      <span className="ml-auto text-xs text-slate-500">{connection}</span>
    </div>
  );
}
