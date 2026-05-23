/**
 * App shell: wires the backend `UiEvent` stream to the terminal and controls, and dispatches
 * commands. Audio and screen capture are handled entirely in Rust (joi-media) — the webview never
 * touches media (PLAN-NATIVE-MEDIA). This component is pure UI + IPC.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Controls } from "./components/Controls";
import { Terminal, type TerminalHandle } from "./components/Terminal";
import {
  commands,
  onUiEvent,
  type AppState,
  type ConnectionStatus,
  type TerminalConfig,
} from "./ipc";

export function App(): React.JSX.Element {
  const [state, setState] = useState<AppState>("stopped");
  const [connection, setConnection] = useState<ConnectionStatus>("disconnected");
  const [micMuted, setMicMuted] = useState(false);
  const [sharing, setSharing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [draft, setDraft] = useState("");
  const [terminalCfg, setTerminalCfg] = useState<TerminalConfig | undefined>();

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

  const sendDraft = useCallback(
    async (e: React.FormEvent) => {
      e.preventDefault();
      const text = draft.trim();
      if (!text) return;
      setDraft("");
      try {
        await commands.sendText({ text });
      } catch (err) {
        terminalRef.current?.writeError(err instanceof Error ? err.message : String(err));
      }
    },
    [draft],
  );

  return (
    <main className="flex h-screen flex-col bg-[#0b0f14]">
      <Controls
        state={state}
        connection={connection}
        micMuted={micMuted}
        sharing={sharing}
        busy={busy}
        onStart={() => void start()}
        onStop={() => void stop()}
        onToggleMute={toggleMute}
        onToggleShare={toggleShare}
      />
      <div className="min-h-0 flex-1 p-2">
        <Terminal ref={terminalRef} terminal={terminalCfg} />
      </div>
      <form onSubmit={sendDraft} className="flex gap-2 border-t border-slate-800 p-2">
        <input
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder={running ? "Type a message…" : "Press Start to begin"}
          disabled={!running}
          className="flex-1 rounded bg-slate-900 px-3 py-2 text-sm text-slate-200 placeholder:text-slate-600 disabled:opacity-50"
        />
        <button
          type="submit"
          disabled={!running || draft.trim() === ""}
          className="rounded bg-emerald-600 px-3 py-2 text-sm text-white disabled:opacity-50"
        >
          Send
        </button>
      </form>
    </main>
  );
}
