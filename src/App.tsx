/**
 * App shell: wires the backend `UiEvent` stream to the terminal and controls, and dispatches
 * commands. Audio and screen capture are handled entirely in Rust (joi-media) — the webview never
 * touches media (PLAN-NATIVE-MEDIA). This component is pure UI + IPC.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Controls } from "./components/Controls";
import { Terminal, type TerminalHandle } from "./components/Terminal";
import { commands, onUiEvent, type AppState, type ConnectionStatus } from "./ipc";

export function App(): React.JSX.Element {
  const [state, setState] = useState<AppState>("stopped");
  const [connection, setConnection] = useState<ConnectionStatus>("disconnected");
  const [micMuted, setMicMuted] = useState(false);
  const [sharing, setSharing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [draft, setDraft] = useState("");

  const running = state !== "stopped" && state !== "error";
  const terminalRef = useRef<TerminalHandle>(null);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void onUiEvent((ev) => {
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
    }).then((u) => {
      unlisten = u;
    });
    return () => unlisten?.();
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

  const toggleMute = useCallback(() => {
    setMicMuted((prev) => {
      const next = !prev;
      void commands.setMicMuted({ muted: next });
      return next;
    });
  }, []);

  const toggleShare = useCallback(() => {
    setSharing((prev) => {
      const next = !prev;
      void (next ? commands.startScreenshare() : commands.stopScreenshare());
      return next;
    });
  }, []);

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
        <Terminal ref={terminalRef} />
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
