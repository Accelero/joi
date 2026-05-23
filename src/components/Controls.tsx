/**
 * Session controls (start/stop, mute, screen-share). Buttons are disabled while a start/stop
 * transition is in flight so the user can't double-fire. Pure presentation: the lifecycle state now
 * lives in the terminal's status line (see `Prompt`), and the connection readout in the deck's
 * bottom rail (App) — this row owns the action buttons only.
 */
import type { AppState } from "../ipc";
import { MicIcon, MicOffIcon, MonitorIcon, PlayIcon, StopIcon } from "./icons";

interface ControlsProps {
  state: AppState;
  micMuted: boolean;
  sharing: boolean;
  busy: boolean;
  onStart(): void;
  onStop(): void;
  onToggleMute(): void;
  onToggleShare(): void;
}

export function Controls({
  state,
  micMuted,
  sharing,
  busy,
  onStart,
  onStop,
  onToggleMute,
  onToggleShare,
}: ControlsProps): React.JSX.Element {
  const running = state !== "stopped" && state !== "error";

  return (
    <div className="controls">
      {running ? (
        <button
          type="button"
          onClick={onStop}
          disabled={busy}
          className="hud-btn hud-btn--icon is-danger"
          aria-label="Stop session"
          title="Stop"
        >
          <StopIcon />
        </button>
      ) : (
        <button
          type="button"
          onClick={onStart}
          disabled={busy}
          className="hud-btn hud-btn--icon is-primary"
          aria-label="Start session"
          title="Start"
        >
          <PlayIcon />
        </button>
      )}

      <button
        type="button"
        onClick={onToggleMute}
        className={`hud-btn hud-btn--icon${micMuted ? " is-danger" : ""}`}
        aria-label={micMuted ? "Unmute microphone" : "Mute microphone"}
        aria-pressed={micMuted}
        title={micMuted ? "Unmute" : "Mute"}
      >
        {micMuted ? <MicOffIcon /> : <MicIcon />}
      </button>

      <button
        type="button"
        onClick={onToggleShare}
        disabled={!running}
        className={`hud-btn hud-btn--icon${sharing ? " is-on" : ""}`}
        aria-label={sharing ? "Stop sharing screen" : "Share screen"}
        aria-pressed={sharing}
        title={sharing ? "Stop sharing" : "Share screen"}
      >
        <MonitorIcon />
      </button>
    </div>
  );
}
