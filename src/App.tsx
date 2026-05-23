/**
 * App shell: wires the backend `UiEvent` stream to the terminal and controls, and dispatches
 * commands. Audio and screen capture are handled entirely in Rust (joi-media) — the webview never
 * touches media (PLAN-NATIVE-MEDIA). This component is pure UI + IPC.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Controls } from "./components/Controls";
import { CloseIcon, MaximizeIcon, MinimizeIcon } from "./components/icons";
import { Prompt } from "./components/Prompt";
import { Terminal, type TerminalHandle } from "./components/Terminal";
import { windowControls } from "./window";
import {
  commands,
  onUiEvent,
  type AppState,
  type ConnectionStatus,
  type TerminalCfg,
} from "./ipc";

// Connection states reuse the same desaturated accent vocabulary as the lifecycle states.
const CONN_COLOR: Record<ConnectionStatus, string> = {
  disconnected: "var(--color-fg-faint)",
  connecting: "var(--color-warn)",
  connected: "var(--color-accent)",
  reconnecting: "var(--color-warn)",
};

function formatDuration(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(Math.floor(total / 3600))}:${p(Math.floor((total % 3600) / 60))}:${p(total % 60)}`;
}

export function App(): React.JSX.Element {
  const [state, setState] = useState<AppState>("stopped");
  const [connection, setConnection] = useState<ConnectionStatus>("disconnected");
  const [micMuted, setMicMuted] = useState(false);
  const [sharing, setSharing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [terminalCfg, setTerminalCfg] = useState<TerminalCfg | undefined>();

  const running = state !== "stopped" && state !== "error";
  const terminalRef = useRef<TerminalHandle>(null);

  // One listener for the app's lifetime: App is the root component, so this is registered once and
  // lives the whole session, turning every backend `ui_event` into a state update or terminal
  // write. Cleanup runs only on teardown (or StrictMode's dev-only remount); holding the
  // registration promise lets us unlisten even if it resolves after cleanup — so no duplicate leaks.
  useEffect(() => {
    const ready = onUiEvent((ev) => {
      switch (ev.type) {
        case "state":
          setState(ev.state);
          if (ev.state === "stopped" || ev.state === "error") {
            setSharing(false);
            void commands.stopScreenshare();
          }
          break;
        case "transcript":
          terminalRef.current?.writeTranscript(ev.speaker, ev.text, ev.final);
          break;
        case "connection":
          setConnection(ev.status);
          break;
        case "error":
          terminalRef.current?.writeError(`${ev.kind}: ${ev.message}`);
          break;
        case "history":
          break;
      }
    });
    return () => void ready.then((unlisten) => unlisten());
  }, []);

  // Pull the configured terminal appearance from the backend once (config lives in Rust; the
  // webview only renders it). Failure is non-fatal — Terminal falls back to its defaults.
  useEffect(() => {
    void commands
      .getUiConfig()
      .then((cfg) => setTerminalCfg(cfg.terminal))
      .catch(() => {});
  }, []);

  // Display-only chrome: a wall clock and the live session uptime for the deck rails. Pure
  // presentation (no business logic) — one timer ticks `now`, uptime is derived from when the
  // session last entered a running state.
  const [now, setNow] = useState(() => Date.now());
  const startedAtRef = useRef<number | null>(null);
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);
  useEffect(() => {
    if (running) startedAtRef.current ??= Date.now();
    else startedAtRef.current = null;
  }, [running]);
  const clock = new Date(now).toLocaleTimeString("en-GB", { hour12: false });
  const uptime = startedAtRef.current !== null ? formatDuration(now - startedAtRef.current) : "--:--:--";

  const start = useCallback(async () => {
    setBusy(true);
    try {
      await commands.start({ resume: false });
    } catch (e) {
      terminalRef.current?.writeError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, []);

  const stop = useCallback(async () => {
    setBusy(true);
    try {
      await commands.stop({ pause: false });
    } catch (e) {
      terminalRef.current?.writeError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, []);

  // Keep the IPC call out of the setState updater: React (StrictMode, dev) double-invokes updaters
  // to surface impurity, which would fire the command twice (e.g. two screen-capture threads).
  const toggleMute = useCallback(() => {
    const next = !micMuted;
    void commands.setMicMuted({ muted: next });
    setMicMuted(next);
  }, [micMuted]);

  const toggleShare = useCallback(() => {
    const next = !sharing;
    void (next ? commands.startScreenshare() : commands.stopScreenshare());
    setSharing(next);
  }, [sharing]);

  const handleSend = useCallback((text: string) => {
    // Echo the typed message into the transcript feed immediately (the backend doesn't round-trip
    // typed input as a transcript event, unlike spoken audio which the provider transcribes).
    terminalRef.current?.writeTranscript("user", text, true);
    void commands
      .sendText({ text })
      .catch((err) =>
        terminalRef.current?.writeError(err instanceof Error ? err.message : String(err)),
      );
  }, []);

  return (
    <main className="flex h-screen">
      <div className="deck flex min-h-0 flex-1 flex-col">
        <span className="deck-corner tl" aria-hidden />
        <span className="deck-corner tr" aria-hidden />
        <span className="deck-corner bl" aria-hidden />
        <span className="deck-corner br" aria-hidden />

        {/* Custom titlebar: the OS decorations are off (decorations:false) so this rail is the drag
            handle, and the window controls on the right replace the native min/max/close. */}
        <header className="deck-top" data-tauri-drag-region="">
          <div className="brand" data-tauri-drag-region="">
            <span className="brand-bar" aria-hidden />
            <span className="brand-name">JOI</span>
            <span className="brand-tag">voice · screen companion</span>
          </div>
          <span className="deck-clock" data-tauri-drag-region="">
            {clock}
          </span>
          <div className="win-controls">
            <button
              type="button"
              className="win-btn"
              onClick={windowControls.minimize}
              aria-label="Minimize window"
              title="Minimize"
            >
              <MinimizeIcon size={15} />
            </button>
            <button
              type="button"
              className="win-btn"
              onClick={windowControls.toggleMaximize}
              aria-label="Maximize window"
              title="Maximize"
            >
              <MaximizeIcon size={13} />
            </button>
            <button
              type="button"
              className="win-btn win-btn--close"
              onClick={windowControls.close}
              aria-label="Close window"
              title="Close"
            >
              <CloseIcon size={15} />
            </button>
          </div>
        </header>

        <Controls
          state={state}
          micMuted={micMuted}
          sharing={sharing}
          busy={busy}
          onStart={() => void start()}
          onStop={() => void stop()}
          onToggleMute={toggleMute}
          onToggleShare={toggleShare}
        />

        <section className="term-panel">
          <div className="panel-label">
            transcript
            <span className="panel-rule" aria-hidden />
          </div>
          <div className="term-host">
            <Terminal ref={terminalRef} terminal={terminalCfg} />
          </div>
          <Prompt state={state} canSend={running} onSend={handleSend} />
        </section>

        <footer className="deck-bottom">
          <span className="inline-flex items-center gap-2" style={{ color: CONN_COLOR[connection] }}>
            <span className="state-dot" style={{ width: 6, height: 6 }} />
            <span style={{ color: "var(--color-fg-faint)" }}>{connection}</span>
          </span>
          <span className="controls-spacer" />
          <span>↑ {uptime}</span>
        </footer>
      </div>
    </main>
  );
}
