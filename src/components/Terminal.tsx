/**
 * The conversation terminal (SPEC §8.2): an xterm.js surface that renders streaming transcripts
 * with per-speaker ANSI colors. Partial lines are rewritten in place (`\x1b[2K\r`) and committed
 * with a newline when finalized. Media never touches this component — only text.
 */
import { useEffect, useImperativeHandle, useRef, type Ref } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import type { Speaker } from "../ipc";

/** Imperative surface the parent drives from `UiEvent`s. */
export interface TerminalHandle {
  /** Write/replace a transcript line. Partials rewrite in place; `final` commits with a newline. */
  writeTranscript(speaker: Speaker, text: string, final: boolean): void;
  /** Write a surfaced error line. */
  writeError(message: string): void;
  /** Clear the screen. */
  clear(): void;
}

const SPEAKER_COLOR: Record<Speaker, string> = {
  user: "\x1b[36m", // cyan
  agent: "\x1b[32m", // green
};
const RESET = "\x1b[0m";

export function Terminal({ ref }: { ref?: Ref<TerminalHandle> }): React.JSX.Element {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<XTerm | null>(null);
  // Speaker of the line currently being streamed, or null when no line is open.
  const openRef = useRef<Speaker | null>(null);

  useImperativeHandle(
    ref,
    (): TerminalHandle => ({
      writeTranscript(speaker, text, final) {
        const term = termRef.current;
        if (!term) return;
        // Close the previous line if the speaker changed mid-stream.
        if (openRef.current !== null && openRef.current !== speaker) {
          term.write("\r\n");
          openRef.current = null;
        }
        const label = speaker === "user" ? "you" : "joi";
        term.write(`\x1b[2K\r${SPEAKER_COLOR[speaker]}${label}${RESET}  ${text}`);
        if (final) {
          term.write("\r\n");
          openRef.current = null;
        } else {
          openRef.current = speaker;
        }
      },
      writeError(message) {
        const term = termRef.current;
        if (!term) return;
        if (openRef.current !== null) {
          term.write("\r\n");
          openRef.current = null;
        }
        term.write(`\x1b[31m! ${message}${RESET}\r\n`);
      },
      clear() {
        termRef.current?.clear();
        openRef.current = null;
      },
    }),
    [],
  );

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const term = new XTerm({
      fontFamily: "JetBrains Mono, monospace",
      fontSize: 14,
      cursorBlink: false,
      disableStdin: true,
      theme: { background: "#0b0f14", foreground: "#cbd5e1" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(container);
    fit.fit();
    term.writeln("\x1b[2mJoi ready — press Start to talk.\x1b[0m");
    termRef.current = term;

    const onResize = () => fit.fit();
    window.addEventListener("resize", onResize);
    return () => {
      window.removeEventListener("resize", onResize);
      term.dispose();
      termRef.current = null;
    };
  }, []);

  return <div ref={containerRef} className="h-full w-full" />;
}
